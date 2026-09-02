//! Reconcile (spec-file-tracking): synchronises the database with the
//! filesystem on demand. The fingerprint phase recovers moved files; new
//! files get metarecords; orphaned metarecords keep their stale path (reconcile
//! never writes Nothing).

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use uuid::Uuid;

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_core::sync::MutexExt;

use crate::db;
use crate::eligibility;
use crate::error::ApiError;
use crate::executor::ensure_parent_metarecords;
use crate::fingerprint;
use crate::fs_meta;
use crate::log::{OpType, Writer};
use crate::relpath::{file_name_bytes, RelPath};
use crate::similarity::{similarity_score, FileSig};
use crate::state::RepoState;
use crate::tree_cache::TreeCache;

#[derive(Debug, Serialize)]
pub struct CandidateMatch {
    pub path: String,
    /// `"partial_hash"` (strong), `"size"` (weak), or `"similarity"` (v2).
    pub fingerprint: &'static str,
    /// Similarity score in [0, 1] for `"similarity"` matches (spec-file-tracking
    /// "File Similarity"); absent for fingerprint matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Candidate {
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub metarecord_uuid: Uuid,
    pub stale_path: String,
    pub matches: Vec<CandidateMatch>,
}

#[derive(Debug, Default, Serialize)]
pub struct ReconcileResult {
    pub created: usize,
    pub moved: usize,
    pub candidates: Vec<Candidate>,
}

/// Full reconcile without the similarity, MIME, metadata or refresh phases
/// (v1 behaviour).
pub fn reconcile(repo: &RepoState) -> Result<ReconcileResult, ApiError> {
    reconcile_full(repo, None, false, false, false)
}

/// Progress is reported every this many items inside the heavy reconcile loops,
/// to bound the cost of progress updates on large repositories.
const PROGRESS_STEP: usize = 128;

/// Cooperative cancellation probe: returns `true` once the task has been asked
/// to stop (spec-tasks "Cancellation"). Checked alongside the progress
/// checkpoints; when it returns `true` the reconcile bails early, dropping its
/// `Writer` so the in-progress transaction rolls back.
pub type CancelProbe<'a> = &'a dyn Fn() -> bool;

/// Phase progress sink: `(phase, done, total)`, with `done`/`total` absent when
/// the phase cannot place a cursor (spec-tasks "Display" renders those as an
/// indeterminate spinner). Reported at phase boundaries and, inside the heavy
/// loops, throttled to every [`PROGRESS_STEP`] items.
pub type ProgressFn<'a> = &'a dyn Fn(&str, Option<u64>, Option<u64>);

/// The two callbacks a reported reconcile carries: where to report phase
/// progress, and how to learn the task was cancelled. They always travel
/// together — one pair per task — so they are passed as one.
pub struct Reporter<'a> {
    progress: ProgressFn<'a>,
    cancel: CancelProbe<'a>,
}

impl<'a> Reporter<'a> {
    pub fn new(progress: ProgressFn<'a>, cancel: CancelProbe<'a>) -> Self {
        Self { progress, cancel }
    }

    fn progress(&self, phase: &str, done: Option<u64>, total: Option<u64>) {
        (self.progress)(phase, done, total);
    }

    fn is_cancelled(&self) -> bool {
        (self.cancel)()
    }
}

/// A reconcile that never cancels (used by the synchronous, non-task wrappers).
fn never() -> impl Fn() -> bool {
    || false
}

/// The error a reconcile returns when it observes a cancellation request and
/// unwinds. The route maps the task to `cancelled` from the registry flag, so
/// this message is not surfaced to a client for the (async) reconcile.
fn cancelled() -> ApiError {
    ApiError::conflict("reconcile cancelled")
}

/// Full reconcile: walk the repository root and synchronise the database.
/// Everything runs in a single transaction (one revision). When `threshold`
/// is `Some`, the v2 similarity phase runs after fingerprinting, appending
/// score-based candidates for still-unmatched orphans and new files
/// (spec-file-tracking "File Similarity"). When `compute_mime` is set, files
/// without an `mfr_mime` get one from content analysis (spec-platform "MIME
/// detection"). When `compute_metadata` is set, files without an
/// `mfr_meta_extracted` marker get their embedded metadata extracted into
/// `mfr_meta_*` fields (spec-platform "Embedded metadata extraction"). When
/// `refresh` is set, files and directories still at their recorded path get
/// their stat-derived `mfr_*` fields refreshed (catching in-place edits made
/// while the watcher was not running), the same way single-metarecord reconcile
/// does.
pub fn reconcile_full(
    repo: &RepoState,
    threshold: Option<f64>,
    compute_mime: bool,
    compute_metadata: bool,
    refresh: bool,
) -> Result<ReconcileResult, ApiError> {
    reconcile_full_reported(
        repo,
        threshold,
        compute_mime,
        compute_metadata,
        refresh,
        &Reporter::new(&|_, _, _| {}, &never()),
    )
}

/// Like [`reconcile_full`], reporting phase progress through `progress`
/// (`phase`, `done`, `total`) so the caller can surface it on a task
/// (spec-tasks). Counts are reported at phase boundaries and, for the heavy
/// loops, throttled to every [`PROGRESS_STEP`] items.
pub fn reconcile_full_reported(
    repo: &RepoState,
    threshold: Option<f64>,
    compute_mime: bool,
    compute_metadata: bool,
    refresh: bool,
    reporter: &Reporter,
) -> Result<ReconcileResult, ApiError> {
    let mut conn = repo.conn.lock_recover();
    let mut cache = repo.lock_cache();
    let root = repo.config.root.clone();
    let mut writer = Writer::begin(&mut conn, None)?;
    let mut result = ReconcileResult::default();

    // Step 2 — pure walk: collect eligible paths (no stat), BFS by depth.
    let internal_dir = repo.internal_dir();
    let mut elig = eligibility::EligibilityCache::default();
    // Declared mount points with nothing mounted on them: their subtrees are
    // frozen — not walked, not orphaned, not offered as candidates
    // (spec-file-tracking "Offline subtrees").
    let offline = crate::mount::offline(writer.connection(), &mut cache, &root)?;
    let paths = walk(
        &mut writer,
        &mut cache,
        &root,
        &internal_dir,
        &RelPath::root(),
        &mut elig,
        &offline,
        reporter,
    )?;

    // Stat phase: the total is now known, so this (the heavy syscall pass) is a
    // determinate phase (spec-tasks "Decompose walk").
    let fs_paths = stat_paths(&root, &paths, reporter);
    if reporter.is_cancelled() {
        return Err(cancelled());
    }

    // New files: paths with no metarecord at that tree position. The regular
    // files (existing or new) are kept for the optional MIME pass below;
    // `fs_paths` is kept whole for the optional refresh pass. Now the total is
    // known, so this indexing pass reports a determinate "index" phase.
    let index_total = fs_paths.len() as u64;
    reporter.progress("index", Some(0), Some(index_total));
    let mut new_files: Vec<(RelPath, Metadata)> = Vec::new();
    let mut disk_files: Vec<RelPath> = Vec::new();
    for (i, (rel, meta)) in fs_paths.iter().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("index", Some(i as u64), Some(index_total));
            if reporter.is_cancelled() {
                return Err(cancelled());
            }
        }
        if meta.is_file() {
            disk_files.push(rel.clone());
        }
        if cache.resolve_path(writer.connection(), "mfr_path", &rel.display())?.is_none() {
            new_files.push((rel.clone(), meta.clone()));
        }
    }

    // Step 1 — orphaned metarecords: tree position no longer present on disk.
    // (Checked against the disk directly, so that files that merely became
    // ineligible are not mistaken for orphans.) Determinate "scan" phase.
    let tracked = db::all_tracked_metarecords(writer.connection())?;
    let scan_total = tracked.len() as u64;
    reporter.progress("scan", Some(0), Some(scan_total));
    let mut orphans: Vec<(Uuid, String)> = Vec::new();
    for (i, uuid) in tracked.into_iter().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("scan", Some(i as u64), Some(scan_total));
            if reporter.is_cancelled() {
                return Err(cancelled());
            }
        }
        let Some(path) = cache.path_of(writer.connection(), "mfr_path", uuid)? else {
            continue;
        };
        if path.is_empty() {
            continue; // The root entry.
        }
        if offline.contains(&path) {
            continue; // Unavailable, not absent: its existence is unknown.
        }
        if !root.join(path.trim_start_matches('/')).exists() {
            orphans.push((uuid, path));
        }
    }

    // Step 3 — fingerprint phase. Hashes of disk files are computed lazily
    // and memoised across orphans. Per-orphan match lists are kept so the
    // similarity phase can extend them.
    let mut partial_cache: HashMap<RelPath, String> = HashMap::new();
    let mut full_cache: HashMap<RelPath, String> = HashMap::new();
    let mut claimed: HashSet<RelPath> = HashSet::new();

    struct OrphanState {
        uuid: Uuid,
        stale_path: String,
        is_dir: bool,
        size: Option<i64>,
        matches: Vec<CandidateMatch>,
        moved: bool,
    }
    let mut states: Vec<OrphanState> = Vec::with_capacity(orphans.len());
    let orphan_total = orphans.len() as u64;
    reporter.progress("fingerprint", Some(0), Some(orphan_total));

    for (i, (orphan, stale_path)) in orphans.into_iter().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("fingerprint", Some(i as u64), Some(orphan_total));
            if reporter.is_cancelled() {
                return Err(cancelled());
            }
        }
        let is_dir = string_field(&writer, orphan, "mfr_type")?.as_deref() == Some("dir");
        let size = int_field(&writer, orphan, "mfr_size")?;
        let mut state = OrphanState {
            uuid: orphan,
            stale_path,
            is_dir,
            size,
            matches: Vec::new(),
            moved: false,
        };

        // Directories have no fingerprint (matched only by path); orphans with
        // no stored size have no fingerprint either. Both can still be matched
        // by the similarity phase below.
        let size = match size {
            Some(size) if !is_dir => size,
            _ => {
                states.push(state);
                continue;
            }
        };
        let stored_partial = string_field(&writer, orphan, "mfr_partial_hash")?;
        let stored_full = string_field(&writer, orphan, "mfr_full_hash")?;

        let mut definitive: Option<RelPath> = None;
        for (rel, meta) in &new_files {
            if claimed.contains(rel) || !meta.is_file() || meta.len() as i64 != size {
                continue;
            }
            let abs = rel.to_abs(&root);
            match &stored_partial {
                None => state.matches.push(CandidateMatch {
                    path: rel.display(),
                    fingerprint: "size",
                    score: None,
                }),
                Some(stored_partial) => {
                    let partial = match partial_cache.get(rel) {
                        Some(p) => p.clone(),
                        None => {
                            let p = fingerprint::partial_hash(&abs)?;
                            partial_cache.insert(rel.clone(), p.clone());
                            p
                        }
                    };
                    if partial != *stored_partial {
                        continue;
                    }
                    match &stored_full {
                        None => state.matches.push(CandidateMatch {
                            path: rel.display(),
                            fingerprint: "partial_hash",
                            score: None,
                        }),
                        Some(stored_full) => {
                            let full = match full_cache.get(rel) {
                                Some(f) => f.clone(),
                                None => {
                                    let f = fingerprint::full_hash(&abs)?;
                                    full_cache.insert(rel.clone(), f.clone());
                                    f
                                }
                            };
                            if full == *stored_full {
                                definitive = Some(rel.clone());
                                break;
                            }
                        }
                    }
                }
            }
        }

        if let Some(rel) = definitive {
            claimed.insert(rel.clone());
            apply_move(&mut writer, &mut cache, &root, orphan, &rel)?;
            state.moved = true;
            result.moved += 1;
        } else {
            // Fingerprint candidate files wait for confirmation: not auto-created.
            for m in &state.matches {
                claimed.insert(RelPath::from_display(&m.path));
            }
        }
        states.push(state);
    }

    // Step 4 — similarity phase (v2): for each still-unmatched orphan and each
    // still-unmatched new path of the same kind, append score-based candidates.
    if let Some(threshold) = threshold {
        reporter.progress("similarity", None, None);
        if reporter.is_cancelled() {
            return Err(cancelled());
        }
        for state in states.iter_mut().filter(|s| !s.moved) {
            let orphan_sig = FileSig::from_path(&state.stale_path, state.size);
            for (rel, meta) in &new_files {
                if claimed.contains(rel) || meta.is_dir() != state.is_dir {
                    continue;
                }
                let new_size = meta.is_file().then_some(meta.len() as i64);
                let score =
                    similarity_score(&orphan_sig, &FileSig::from_path(&rel.display(), new_size));
                if score >= threshold {
                    state.matches.push(CandidateMatch {
                        path: rel.display(),
                        fingerprint: "similarity",
                        score: Some(score),
                    });
                    claimed.insert(rel.clone()); // Candidate: not auto-created.
                }
            }
        }
    }

    for state in states {
        if !state.moved && !state.matches.is_empty() {
            result.candidates.push(Candidate {
                metarecord_uuid: state.uuid,
                stale_path: state.stale_path,
                matches: state.matches,
            });
        }
    }

    // Step 5 — create metarecords for the remaining new files, parents first.
    new_files.sort_by_key(|(rel, _)| rel.depth());
    let create_total = new_files.len() as u64;
    reporter.progress("create", Some(0), Some(create_total));
    for (i, (rel, _)) in new_files.iter().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("create", Some(i as u64), Some(create_total));
            if reporter.is_cancelled() {
                return Err(cancelled());
            }
        }
        if claimed.contains(rel) {
            continue;
        }
        if cache.resolve_path(writer.connection(), "mfr_path", &rel.display())?.is_some() {
            continue; // Already created as a parent of an earlier path.
        }
        create_record_for(&mut writer, &mut cache, &root, rel, &[], compute_mime)?;
        result.created += 1;
    }

    // Step 5b — refresh phase (option): every file/directory still at its
    // recorded path gets its stat-derived `mfr_*` fields refreshed, catching
    // in-place edits made while the watcher was not running. Records just
    // created or moved above already hold current stat fields, so
    // `refresh_stat_fields` (which writes only changed fields) is a no-op for
    // them. Same behaviour as single-metarecord reconcile.
    if refresh {
        let refresh_total = fs_paths.len() as u64;
        reporter.progress("refresh", Some(0), Some(refresh_total));
        for (i, (rel, _)) in fs_paths.iter().enumerate() {
            if i % PROGRESS_STEP == 0 {
                reporter.progress("refresh", Some(i as u64), Some(refresh_total));
                if reporter.is_cancelled() {
                    return Err(cancelled());
                }
            }
            if let Some(uuid) =
                cache.resolve_path(writer.connection(), "mfr_path", &rel.display())?
            {
                refresh_stat_fields(&mut writer, &root, uuid, rel)?;
            }
        }
    }

    // Step 6 — MIME phase (spec-platform): every eligible file on disk now has
    // a record; fill in mfr_mime where it is still absent.
    if compute_mime {
        let mime_total = disk_files.len() as u64;
        reporter.progress("mime", Some(0), Some(mime_total));
        for (i, rel) in disk_files.iter().enumerate() {
            if i % PROGRESS_STEP == 0 {
                reporter.progress("mime", Some(i as u64), Some(mime_total));
                if reporter.is_cancelled() {
                    return Err(cancelled());
                }
            }
            if let Some(uuid) =
                cache.resolve_path(writer.connection(), "mfr_path", &rel.display())?
            {
                maybe_compute_mime(&mut writer, &root, uuid, rel)?;
            }
        }
    }

    // Step 7 — metadata phase (spec-platform): extract embedded `mfr_meta_*`
    // fields for files not yet marked `mfr_meta_extracted`.
    if compute_metadata {
        let map = repo.metadata_map.lock_recover().clone();
        let meta_total = disk_files.len() as u64;
        reporter.progress("metadata", Some(0), Some(meta_total));
        for (i, rel) in disk_files.iter().enumerate() {
            if i % PROGRESS_STEP == 0 {
                reporter.progress("metadata", Some(i as u64), Some(meta_total));
                if reporter.is_cancelled() {
                    return Err(cancelled());
                }
            }
            if let Some(uuid) =
                cache.resolve_path(writer.connection(), "mfr_path", &rel.display())?
            {
                maybe_extract_metadata(&mut writer, &root, uuid, rel, &map)?;
            }
        }
    }

    writer.commit()?;
    Ok(result)
}

/// Single-metarecord reconcile: same semantics scoped to the subtree rooted at
/// the given metarecord, without the fingerprint phase. When `refresh` is set,
/// existing metarecords still at their recorded path get their `mfr_*` stat
/// fields refreshed (same option as full reconcile).
pub fn reconcile_metarecord(
    repo: &RepoState,
    uuid: Uuid,
    compute_mime: bool,
    compute_metadata: bool,
    refresh: bool,
) -> Result<ReconcileResult, ApiError> {
    reconcile_metarecord_reported(
        repo,
        uuid,
        compute_mime,
        compute_metadata,
        refresh,
        &Reporter::new(&|_, _, _| {}, &never()),
    )
}

/// Like [`reconcile_metarecord`], reporting phase progress for a task
/// (spec-tasks). Phases: `walk` (subtree), `create` (create/refresh over the
/// subtree), `mime`, `metadata`.
pub fn reconcile_metarecord_reported(
    repo: &RepoState,
    uuid: Uuid,
    compute_mime: bool,
    compute_metadata: bool,
    refresh: bool,
    reporter: &Reporter,
) -> Result<ReconcileResult, ApiError> {
    let mut conn = repo.conn.lock_recover();
    let mut cache = repo.lock_cache();
    let root = repo.config.root.clone();

    if db::get_version(&conn, uuid)?.is_none() {
        return Err(ApiError::not_found(format!("Metarecord not found: {uuid}")));
    }
    let Some(base) = cache.path_of(&conn, "mfr_path", uuid)? else {
        return Err(ApiError::bad_request(format!(
            "entry {uuid} has no valid mfr_path (Nothing or unknown)"
        )));
    };

    let offline = crate::mount::offline(&conn, &mut cache, &root)?;
    if offline.contains(&base) {
        // Aimed at (or into) a volume that is not plugged in. Doing nothing
        // silently would look like "reconcile found no change"; say so instead
        // — this is exactly the operation the user runs once the drive is back
        // (spec-file-tracking "Offline subtrees").
        return Err(ApiError::bad_request(format!(
            "{base} is on a volume that is not mounted: nothing to reconcile until it is back"
        )));
    }

    let mut writer = Writer::begin(&mut conn, None)?;
    let mut result = ReconcileResult::default();

    // Pure walk of the subtree (BFS, no stat) then the determinate stat phase,
    // same shape as the whole-repository reconcile (spec-tasks).
    let mut elig = eligibility::EligibilityCache::default();
    let mut paths: Vec<RelPath> = Vec::new();
    let base_rel = RelPath::from_display(&base);
    let abs_base = base_rel.to_abs(&root);
    if abs_base.exists() {
        paths.push(base_rel.clone()); // The subtree root itself.
        paths.extend(walk(
            &mut writer,
            &mut cache,
            &root,
            &repo.internal_dir(),
            &base_rel,
            &mut elig,
            &offline,
            reporter,
        )?);
    }
    let mut fs_paths = stat_paths(&root, &paths, reporter);
    if reporter.is_cancelled() {
        return Err(cancelled());
    }

    fs_paths.sort_by_key(|(rel, _)| rel.depth());
    let create_total = fs_paths.len() as u64;
    reporter.progress("create", Some(0), Some(create_total));
    for (i, (rel, _)) in fs_paths.iter().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("create", Some(i as u64), Some(create_total));
            if reporter.is_cancelled() {
                return Err(cancelled());
            }
        }
        // The subtree root itself was made eligible by the caller setting
        // mf_watch directly; descendants were eligibility-checked by walk().
        match cache.resolve_path(writer.connection(), "mfr_path", &rel.display())? {
            Some(existing) => {
                if refresh {
                    refresh_stat_fields(&mut writer, &root, existing, rel)?;
                }
            }
            None => {
                create_record_for(&mut writer, &mut cache, &root, rel, &[], compute_mime)?;
                result.created += 1;
            }
        }
    }

    if compute_mime {
        let mime_total = fs_paths.len() as u64;
        reporter.progress("mime", Some(0), Some(mime_total));
        for (i, (rel, meta)) in fs_paths.iter().enumerate() {
            if i % PROGRESS_STEP == 0 {
                reporter.progress("mime", Some(i as u64), Some(mime_total));
                if reporter.is_cancelled() {
                    return Err(cancelled());
                }
            }
            if !meta.is_file() {
                continue;
            }
            if let Some(uuid) =
                cache.resolve_path(writer.connection(), "mfr_path", &rel.display())?
            {
                maybe_compute_mime(&mut writer, &root, uuid, rel)?;
            }
        }
    }

    if compute_metadata {
        let map = repo.metadata_map.lock_recover().clone();
        let meta_total = fs_paths.len() as u64;
        reporter.progress("metadata", Some(0), Some(meta_total));
        for (i, (rel, meta)) in fs_paths.iter().enumerate() {
            if i % PROGRESS_STEP == 0 {
                reporter.progress("metadata", Some(i as u64), Some(meta_total));
                if reporter.is_cancelled() {
                    return Err(cancelled());
                }
            }
            if !meta.is_file() {
                continue;
            }
            if let Some(uuid) =
                cache.resolve_path(writer.connection(), "mfr_path", &rel.display())?
            {
                maybe_extract_metadata(&mut writer, &root, uuid, rel, &map)?;
            }
        }
    }

    writer.commit()?;
    Ok(result)
}

// ── Shared helpers (also used by the track endpoint) ──────────────────────────

/// Recursively walks `prefix` (repo-root-relative), collecting eligible
/// paths. Ineligible directories are pruned (cascading skip); the
/// repository's `.metafolder/internal/` directory is always skipped,
/// matched by absolute path (the metafolder may live anywhere).
/// Pure filesystem traversal (spec-tasks "Decompose walk"): collects every
/// eligible repo-root-relative path under `base` *without* stat'ing it, BFS by
/// depth so progress can be reported per level (the current depth and how far
/// through the level we are). Stat'ing the paths happens afterwards in
/// [`stat_paths`], once the total is known — so the heavy syscall pass gets an
/// exact progress bar. Returns the collected paths (files and directories);
/// `base` itself is not included.
#[allow(clippy::too_many_arguments)]
fn walk(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    internal_dir: &Path,
    base: &RelPath,
    elig: &mut eligibility::EligibilityCache,
    offline: &crate::mount::OfflineMounts,
    reporter: &Reporter,
) -> Result<Vec<RelPath>> {
    let mut paths: Vec<RelPath> = Vec::new();
    // The two lists the traversal alternates between: the directories at the
    // current depth, and those discovered for the next.
    let mut frontier: Vec<RelPath> = vec![base.clone()];
    let mut depth: u64 = 0;
    // Files seen across the *whole* walk so far. Reported in the phase label so
    // the displayed count actually advances: the per-depth `done/total`
    // (directories at this depth) is too coarse — a level usually holds fewer
    // than `PROGRESS_STEP` directories, so `done` never leaves 0.
    let mut files: u64 = 0;
    while !frontier.is_empty() {
        let dirs_at_depth = frontier.len() as u64;
        let label =
            |files: u64| format!("walk (depth {depth}, {dirs_at_depth} dirs, {files} files)");
        let mut next: Vec<RelPath> = Vec::new();
        for (i, dir) in frontier.iter().enumerate() {
            if (i as u64).is_multiple_of(PROGRESS_STEP as u64) {
                reporter.progress(&label(files), Some(i as u64), Some(dirs_at_depth));
                if reporter.is_cancelled() {
                    anyhow::bail!("reconcile cancelled");
                }
            }
            let abs = dir.to_abs(root);
            let entries = match std::fs::read_dir(&abs) {
                Ok(entries) => entries,
                Err(_) => continue, // Not a directory or unreadable.
            };
            for entry in entries {
                let entry = entry?;
                // A POSIX name is a byte string, and the daemon tracks such a
                // file like any other (spec-data-model "Tree names").
                let name = TreeName::from_bytes(file_name_bytes(&entry.file_name()));
                if entry.path() == internal_dir {
                    continue;
                }
                let rel = dir.child(name);
                // Ignore patterns are regexes over text, so they see the
                // displayed path — the same one the user wrote them against.
                let display = rel.display();
                if !eligibility::is_eligible_cached(writer.connection(), cache, &display, elig)? {
                    continue;
                }
                // `file_type` is free here (from the dir entry, no stat).
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                paths.push(rel.clone());
                if is_dir {
                    // An offline mount point is itself an ordinary directory
                    // (it exists, its own metadata is real); what is behind it
                    // is not there to be walked.
                    if offline.contains(&display) {
                        continue;
                    }
                    next.push(rel);
                } else {
                    files += 1;
                    // Keep the count moving and cancellation responsive even
                    // inside a single huge directory (the per-dir checkpoint
                    // above would not fire until the next directory).
                    if files.is_multiple_of(PROGRESS_STEP as u64) {
                        reporter.progress(&label(files), Some(i as u64), Some(dirs_at_depth));
                        if reporter.is_cancelled() {
                            anyhow::bail!("reconcile cancelled");
                        }
                    }
                }
            }
        }
        // End-of-level tick: the cumulative file count now includes this whole
        // depth, so the reported number reaches the true total for small trees.
        reporter.progress(&label(files), Some(dirs_at_depth), Some(dirs_at_depth));
        frontier = next;
        depth += 1;
    }
    Ok(paths)
}

/// Stats each path from the pure [`walk`], building the `(path, Metadata)` list
/// the rest of reconcile consumes. The total is known, so this — the heavy
/// syscall pass — reports a determinate "stat" phase. Paths that vanished
/// between the walk and the stat are skipped.
fn stat_paths(root: &Path, paths: &[RelPath], reporter: &Reporter) -> Vec<(RelPath, Metadata)> {
    let total = paths.len() as u64;
    reporter.progress("stat", Some(0), Some(total));
    let mut out: Vec<(RelPath, Metadata)> = Vec::with_capacity(paths.len());
    for (i, rel) in paths.iter().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("stat", Some(i as u64), Some(total));
        }
        let abs = rel.to_abs(root);
        // symlink_metadata matches the former DirEntry::metadata (no symlink follow).
        if let Ok(meta) = std::fs::symlink_metadata(&abs) {
            out.push((rel.clone(), meta));
        }
    }
    out
}

/// Creates the metarecord for a new filesystem path (parents included).
/// When `compute_mime` is set, a detectable file gets its `mfr_mime` as part of
/// the create operation (folded in, so no separate field write / version bump);
/// directories and undetectable files get none (spec-platform "MIME detection").
pub(crate) fn create_record_for(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    rel: &RelPath,
    extra_fields: &[Field],
    compute_mime: bool,
) -> Result<Uuid> {
    let parent = ensure_parent_metarecords(writer, cache, root, rel, extra_fields)?;
    let name = rel.name().cloned().unwrap_or_default();
    let abs = rel.to_abs(root);
    let mut fields =
        vec![Field::new("mfr_path", Value::TreeRef { parent: Some(parent), name: name.clone() })];
    fields.extend(fs_meta::stat_fields_in(root, &abs)?);
    if compute_mime {
        if let Some(mime) = detect_mime(&abs) {
            fields.push(Field::new("mfr_mime", Value::String(mime)));
        }
    }
    fields.extend(extra_fields.iter().cloned());
    let created = writer.create_metarecord(fields)?;
    cache.apply_insert("mfr_path", Some(parent), &name, created.uuid);
    Ok(created.uuid)
}

/// Re-points an orphaned metarecord at its recovered location and refreshes its
/// stat fields.
fn apply_move(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    uuid: Uuid,
    rel: &RelPath,
) -> Result<()> {
    let parent = ensure_parent_metarecords(writer, cache, root, rel, &[])?;
    let name = rel.name().cloned().unwrap_or_default();
    writer.set_field_as(
        OpType::FileMoved,
        uuid,
        "mfr_path",
        Value::TreeRef { parent: Some(parent), name: name.clone() },
    )?;
    cache.apply_remove("mfr_path", uuid);
    cache.apply_insert("mfr_path", Some(parent), &name, uuid);
    refresh_stat_fields(writer, root, uuid, rel)
}

/// Refreshes the stat-derived fields of an existing metarecord, writing only the
/// fields whose value actually changed (idempotent reconciles do not grow
/// the log).
fn refresh_stat_fields(writer: &mut Writer, root: &Path, uuid: Uuid, rel: &RelPath) -> Result<()> {
    let Ok(stat) = fs_meta::stat_fields_in(root, &rel.to_abs(root)) else {
        return Ok(());
    };
    for field in stat {
        let current = db::get_field_rows_named(writer.connection(), uuid, &field.name)?;
        if current.len() == 1 && current[0].value == field.value {
            continue;
        }
        writer.set_field_as(OpType::FileModified, uuid, &field.name, field.value)?;
    }
    Ok(())
}

fn string_field(writer: &Writer, uuid: Uuid, name: &str) -> Result<Option<String>> {
    db::string_field(writer.connection(), uuid, name)
}

fn int_field(writer: &Writer, uuid: Uuid, name: &str) -> Result<Option<i64>> {
    db::int_field(writer.connection(), uuid, name)
}

// ── MIME detection (spec-platform "MIME detection") ─────────────────────────────

/// Content-based MIME detection with the pure-Rust `infer` crate (magic bytes).
/// Returns `None` for unreadable files and for types `infer` cannot recognise
/// (e.g. plain text), leaving `mfr_mime` absent in those cases.
fn detect_mime(abs: &Path) -> Option<String> {
    infer::get_from_path(abs).ok().flatten().map(|t| t.mime_type().to_string())
}

/// Sets `mfr_mime` on a file record that does not have one yet. Idempotent: an
/// existing `mfr_mime` is never recomputed (so re-running reconcile does not
/// grow the log; content changes are out of scope, like the hashes).
fn maybe_compute_mime(writer: &mut Writer, root: &Path, uuid: Uuid, rel: &RelPath) -> Result<()> {
    if !db::get_field_rows_named(writer.connection(), uuid, "mfr_mime")?.is_empty() {
        return Ok(());
    }
    let abs = rel.to_abs(root);
    if let Some(mime) = detect_mime(&abs) {
        writer.set_field_as(OpType::FileModified, uuid, "mfr_mime", Value::String(mime))?;
    }
    Ok(())
}

// ── Embedded metadata (spec-platform "Embedded metadata extraction") ─────────────

/// Extracts embedded `mfr_meta_*` fields for a file that has not been analysed
/// yet, using the per-repo `map`. Idempotent via the `mfr_meta_extracted`
/// marker: a file already carrying it is skipped, so a file that legitimately
/// has *no* metadata is not re-parsed on every reconcile (the marker is set even
/// when nothing was found). Content changes are out of scope — clear the marker
/// (and stale `mfr_meta_*` values) to re-read, the same "clear to recompute"
/// contract as `mfr_mime`.
fn maybe_extract_metadata(
    writer: &mut Writer,
    root: &Path,
    uuid: Uuid,
    rel: &RelPath,
    map: &crate::metadata_map::MetadataMap,
) -> Result<()> {
    if !db::get_field_rows_named(writer.connection(), uuid, "mfr_meta_extracted")?.is_empty() {
        return Ok(());
    }
    let abs = rel.to_abs(root);
    for field in crate::metadata::extract(&abs, map) {
        writer.set_field_as(OpType::FileModified, uuid, &field.name, field.value)?;
    }
    writer.set_field_as(OpType::FileModified, uuid, "mfr_meta_extracted", Value::Bool(true))?;
    Ok(())
}
