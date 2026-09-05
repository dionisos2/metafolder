//! Orphan scan (spec-file-tracking "Orphan scan"): find tracked metarecords
//! whose `mfr_path` points to a filesystem location that is *definitely* gone,
//! and (`clear`) commit that fact by orphaning them — snapshotting
//! `mfr_path_old` and setting `mfr_path` to `Nothing`, cascading to descendants
//! — the same transition the watcher performs on a live delete.
//!
//! Unlike reconcile (which walks the eligible tree and never writes `Nothing`),
//! the scan is driven by the *tracked set*: every metarecord that still claims a
//! path is checked against the disk, regardless of current eligibility, so a
//! subtree deleted while unwatched is still found. It is guarded against false
//! positives: a path is reported only when a readable ancestor directory proves
//! the file is truly absent — an unreadable (EACCES) or missing-mount ancestor
//! yields "unknown", never an orphan (spec-file-tracking "Orphan scan").

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use metafolder_core::metarecord::Value;
use metafolder_core::sync::MutexExt;
use uuid::Uuid;

use crate::db;
use crate::error::ApiError;
use crate::log::{OpType, Writer};
use crate::state::RepoState;

/// One orphaned metarecord: its uuid and the stale path its `mfr_path` still
/// resolves to (root-relative, `mfr_path` form with a leading slash).
#[derive(Debug, Clone, serde::Serialize)]
pub struct OrphanEntry {
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub uuid: Uuid,
    pub stale_path: String,
}

/// Scan the repository for orphaned metarecords (see the module docs). Read-only.
pub fn scan_orphans(repo: &RepoState) -> Result<Vec<OrphanEntry>, ApiError> {
    let conn = repo.conn.lock_recover();
    let mut cache = repo.lock_cache();
    let root = &repo.config.root;

    // Memoise per-directory readability so a subtree costs one `read_dir` per
    // directory, not one per record.
    let mut dirs: HashMap<PathBuf, DirState> = HashMap::new();
    let offline = crate::mount::offline(&conn, &mut cache, root)?;
    let mut orphans = Vec::new();
    for uuid in db::all_tracked_metarecords(&conn)? {
        let Some(path) = cache.path_of(&conn, "mfr_path", uuid)? else {
            continue; // No resolvable path (e.g. already Nothing): not this scan.
        };
        if path.is_empty() {
            continue; // The filesystem root entry.
        }
        if !offline.contains(&path) && is_definitely_gone(root, &path, &mut dirs) {
            orphans.push(OrphanEntry { uuid, stale_path: path });
        }
    }
    Ok(orphans)
}

/// Orphan the given metarecords: for each uuid still pointing at a path that is
/// definitely gone, snapshot `mfr_path_old` and set `mfr_path` to `Nothing`,
/// cascading to every descendant — the same transition the watcher makes on a
/// live delete. A uuid whose file exists again (or is unreadable/unknown) is
/// skipped, so a stale scan never orphans a since-recreated file. All changes go
/// in one revision. Returns how many top-level records were orphaned.
pub fn clear_orphans(repo: &RepoState, uuids: &[Uuid]) -> Result<usize, ApiError> {
    let mut conn = repo.conn.lock_recover();
    let mut cache = repo.lock_cache();
    let root = repo.config.root.clone();
    let mut dirs: HashMap<PathBuf, DirState> = HashMap::new();
    let offline = crate::mount::offline(&conn, &mut cache, &root)?;
    let mut writer = Writer::begin(&mut conn, None)?;
    let mut cleared = 0;

    for &uuid in uuids {
        let Some(path) = cache.path_of(writer.connection(), "mfr_path", uuid)? else {
            continue; // Already Nothing (e.g. cascaded by an earlier iteration).
        };
        if path.is_empty() {
            continue; // Never orphan the filesystem root entry.
        }
        // Re-verify against the disk: a scan is a snapshot, and the file may have
        // returned since. Only orphan what is still definitely gone — and never
        // a path whose volume was unplugged in the meantime.
        if offline.contains(&path) || !is_definitely_gone(&root, &path, &mut dirs) {
            continue;
        }
        // Snapshot every path *before* any write: clearing a parent's `mfr_path`
        // would break its descendants' `path_of` walk (mirrors the watcher's
        // `apply_remove`; spec-file-tracking "Orphan origin").
        let descendants = cache.descendants(writer.connection(), "mfr_path", uuid)?;
        let mut olds = Vec::with_capacity(descendants.len() + 1);
        for &u in std::iter::once(&uuid).chain(descendants.iter()) {
            olds.push((u, cache.path_of(writer.connection(), "mfr_path", u)?));
        }
        for (u, old) in olds {
            if let Some(old) = old {
                writer.set_field_as(OpType::FileDeleted, u, "mfr_path_old", Value::String(old))?;
            }
            writer.set_field_as(OpType::FileDeleted, u, "mfr_path", Value::Nothing)?;
            // Same transition as the watcher's, so it carries the same consequence:
            // an orphan is no longer a live duplicate (spec-duplicates "Invariant").
            writer.clear_field_as(OpType::FileDeleted, u, "mfr_duplicate_group")?;
        }
        cache.apply_remove("mfr_path", uuid);
        cleared += 1;
    }

    writer.commit()?;
    Ok(cleared)
}

/// Whether a directory could be read, was missing, or was unreadable — the three
/// outcomes that decide whether an absent child is "gone" or merely "unknown".
#[derive(Clone, Copy, PartialEq, Eq)]
enum DirState {
    Readable,
    Missing,
    Unreadable,
}

fn dir_state(dir: &Path, cache: &mut HashMap<PathBuf, DirState>) -> DirState {
    if let Some(state) = cache.get(dir) {
        return *state;
    }
    let state = match std::fs::read_dir(dir) {
        Ok(_) => DirState::Readable,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DirState::Missing,
        Err(_) => DirState::Unreadable, // EACCES, ENOTCONN (stale mount), …
    };
    cache.insert(dir.to_path_buf(), state);
    state
}

/// Is the record at `rel` (an `mfr_path`-form path) *definitely* gone from disk?
///
/// `true` only when some ancestor directory is readable and yet `rel` is absent
/// below it — proof the file no longer exists. If the file is present (even a
/// broken symlink), or the nearest non-missing ancestor is unreadable, or the
/// walk climbs out of the repository root without finding a readable ancestor,
/// the answer is `false` (unknown), so a permission drop or an unmounted mount
/// never mass-orphans a subtree (spec-file-tracking "Orphan scan").
fn is_definitely_gone(root: &Path, rel: &str, cache: &mut HashMap<PathBuf, DirState>) -> bool {
    let abs = root.join(rel.trim_start_matches('/'));
    if std::fs::symlink_metadata(&abs).is_ok() {
        return false; // Present on disk (a broken symlink still counts).
    }
    let mut cur = abs.parent();
    while let Some(dir) = cur {
        if !dir.starts_with(root) {
            return false; // Climbed above the repository root → cannot tell.
        }
        match dir_state(dir, cache) {
            DirState::Readable => return true,
            DirState::Missing => cur = dir.parent(), // A deleted directory: keep climbing.
            DirState::Unreadable => return false,
        }
    }
    false
}

// ── Relinking orphans by fingerprint (spec-file-tracking "Relinking orphans") ─

/// What a relink did.
#[derive(Debug, Default, serde::Serialize, PartialEq, Eq)]
pub struct RelinkResult {
    /// Orphans re-homed onto the file that carries their content.
    pub relinked: usize,
    /// Matches left alone because the tracked metarecord holding the file
    /// carries fields of its own — absorbing it would delete them.
    pub conflicts: usize,
    /// Candidates that could not be read (permissions, an offline volume).
    pub skipped: usize,
}

/// Re-homes orphaned metarecords onto files that carry their content.
///
/// This is the operation the watcher's arrival path used to attempt inline. It
/// cannot be done without hashing the candidate files, and one filesystem event
/// can carry a whole subtree, so it belongs to a command the user runs rather
/// than to the event path — where it also almost never paid off, since only the
/// duplicate scan stores the hashes it needs.
///
/// An orphan is a candidate only if it carries both fingerprints (nothing else
/// can confirm identity). A tracked file is a candidate only if it carries no
/// field of its own beyond the `mfr_*` the daemon maintains: absorbing one that
/// the user has annotated would destroy that annotation, so it is reported as a
/// conflict and left alone.
///
/// On a match the *orphan* takes the tracked position and the metarecord that
/// held it is deleted — that way round, because the orphan's uuid is what
/// references elsewhere point at, and preserving it is the whole reason to
/// re-home rather than track afresh.
pub fn relink(repo: &RepoState) -> Result<RelinkResult, ApiError> {
    relink_reported(repo, &crate::tasks::Reporter::silent())
}

/// [`relink`], reporting progress and observing cancellation (spec-tasks).
pub fn relink_reported(
    repo: &RepoState,
    reporter: &crate::tasks::Reporter,
) -> Result<RelinkResult, ApiError> {
    let mut conn = repo.conn.lock_recover();
    let root = repo.config.root.clone();
    let mut result = RelinkResult::default();

    // The orphans that can be confirmed at all, indexed by size: the size is
    // the only thing comparable without reading a file, so it is what keeps the
    // hashing down to the candidates that could possibly match.
    reporter.progress("orphans", None, None);
    let mut by_size: HashMap<i64, Vec<db::OrphanCandidate>> = HashMap::new();
    for candidate in db::hashed_orphans(&conn)? {
        by_size.entry(candidate.size).or_default().push(candidate);
    }
    if by_size.is_empty() {
        return Ok(result);
    }

    // Tracked files whose size matches one of them.
    let tracked: Vec<(Uuid, i64)> = db::tracked_files_with_size(&conn)?
        .into_iter()
        .filter(|(_, size)| by_size.contains_key(size))
        .collect();

    let total = tracked.len() as u64;
    reporter.progress("relink", Some(0), Some(total));
    let mut cache = repo.lock_cache();
    for (done, (uuid, size)) in tracked.into_iter().enumerate() {
        if reporter.is_cancelled() {
            return Err(ApiError::conflict("relink cancelled"));
        }
        reporter.progress("relink", Some(done as u64), Some(total));

        let Some(rel) = cache.path_of(&conn, "mfr_path", uuid)? else { continue };
        let abs = crate::relpath::RelPath::from_display(&rel).to_abs(&root);
        let Some(orphan) = confirm(&abs, by_size.get(&size).map_or(&[][..], |v| v)) else {
            continue;
        };
        // The file's own metarecord must be one the daemon alone wrote.
        if annotated(&conn, uuid)? {
            result.conflicts += 1;
            continue;
        }
        adopt(&mut conn, &mut cache, &root, orphan, uuid, &rel)?;
        // Each orphan re-homes once.
        if let Some(list) = by_size.get_mut(&size) {
            list.retain(|c| c.uuid != orphan);
        }
        result.relinked += 1;
    }
    reporter.progress("relink", Some(total), Some(total));
    Ok(result)
}

/// The orphan whose stored full hash `abs` matches, if any. Hashes lazily: the
/// partial hash first, the full one only once a partial has matched.
fn confirm(abs: &Path, candidates: &[db::OrphanCandidate]) -> Option<Uuid> {
    if candidates.is_empty() {
        return None;
    }
    let partial = crate::fingerprint::partial_hash(abs).ok()?;
    let mut full: Option<String> = None;
    for candidate in candidates {
        if candidate.partial_hash != partial {
            continue;
        }
        if full.is_none() {
            full = Some(crate::fingerprint::full_hash(abs).ok()?);
        }
        if full.as_deref() == Some(candidate.full_hash.as_str()) {
            return Some(candidate.uuid);
        }
    }
    None
}

/// Whether the metarecord carries anything the daemon did not put there. The
/// `mfr_*` namespace is the daemon's (spec-data-model "Reserved fields"), so
/// anything outside it is the user's and must not be deleted.
fn annotated(conn: &rusqlite::Connection, uuid: Uuid) -> Result<bool, ApiError> {
    let record = db::get_metarecord(conn, uuid)?;
    Ok(record.is_some_and(|r| r.fields.iter().any(|f| !f.name.starts_with("mfr_"))))
}

/// Moves `orphan` onto the position `holder` occupies, then deletes `holder`.
fn adopt(
    conn: &mut rusqlite::Connection,
    cache: &mut crate::tree_cache::TreeCache,
    root: &Path,
    orphan: Uuid,
    holder: Uuid,
    rel: &str,
) -> Result<(), ApiError> {
    let mut writer = Writer::begin(conn, None)?;
    // The holder leaves the position first: one metarecord holds a given tree
    // position, so the orphan cannot take it while the holder is still there.
    writer.delete_metarecord(holder)?;
    cache.apply_remove("mfr_path", holder);
    let path = crate::relpath::RelPath::from_display(rel);
    let parent = crate::executor::ensure_parent_metarecords(&mut writer, cache, root, &path, &[])?;
    let name = path.name().cloned().unwrap_or_default();
    writer.set_field_as(
        OpType::FileMoved,
        orphan,
        "mfr_path",
        Value::TreeRef { parent: Some(parent), name: name.clone() },
    )?;
    cache.apply_insert("mfr_path", Some(parent), &name, orphan);
    writer.commit()?;
    Ok(())
}
