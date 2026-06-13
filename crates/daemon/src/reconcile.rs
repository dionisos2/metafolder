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

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::sync::MutexExt;

use crate::db;
use crate::eligibility;
use crate::error::ApiError;
use crate::executor::ensure_parent_metarecords;
use crate::fingerprint;
use crate::fs_meta;
use crate::log::{OpType, Writer};
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

/// Full reconcile without the similarity, MIME or refresh phases (v1 behaviour).
pub fn reconcile(repo: &RepoState) -> Result<ReconcileResult, ApiError> {
    reconcile_full(repo, None, false, false)
}

/// Full reconcile: walk the repository root and synchronise the database.
/// Everything runs in a single transaction (one revision). When `threshold`
/// is `Some`, the v2 similarity phase runs after fingerprinting, appending
/// score-based candidates for still-unmatched orphans and new files
/// (spec-file-tracking "File Similarity"). When `compute_mime` is set, files
/// without an `mfr_mime` get one from content analysis (spec-platform "MIME
/// detection"). When `refresh` is set, files and directories still at their
/// recorded path get their stat-derived `mfr_*` fields refreshed (catching
/// in-place edits made while the watcher was not running), the same way
/// single-metarecord reconcile does.
pub fn reconcile_full(
    repo: &RepoState,
    threshold: Option<f64>,
    compute_mime: bool,
    refresh: bool,
) -> Result<ReconcileResult, ApiError> {
    let mut conn = repo.conn.lock_recover();
    let mut cache = repo.lock_cache();
    let root = repo.config.root.clone();
    let db_id = repo.config.repo_uuid;
    let mut writer = Writer::begin(&mut conn, db_id, None)?;
    let mut result = ReconcileResult::default();

    // Step 2 — walk the filesystem (eligibility-pruned).
    let internal_dir = repo.internal_dir();
    let mut fs_paths: Vec<(String, Metadata)> = Vec::new();
    walk(&mut writer, &mut cache, &root, &internal_dir, "", &mut fs_paths)?;

    // New files: paths with no metarecord at that tree position. The regular
    // files (existing or new) are kept for the optional MIME pass below;
    // `fs_paths` is kept whole for the optional refresh pass.
    let mut new_files: Vec<(String, Metadata)> = Vec::new();
    let mut disk_files: Vec<String> = Vec::new();
    for (rel, meta) in &fs_paths {
        if meta.is_file() {
            disk_files.push(rel.clone());
        }
        if cache.resolve_path(writer.connection(), "mfr_path", rel)?.is_none() {
            new_files.push((rel.clone(), meta.clone()));
        }
    }

    // Step 1 — orphaned metarecords: tree position no longer present on disk.
    // (Checked against the disk directly, so that files that merely became
    // ineligible are not mistaken for orphans.)
    let mut orphans: Vec<(Uuid, String)> = Vec::new();
    for uuid in db::all_tracked_metarecords(writer.connection(), db_id)? {
        let Some(path) = cache.path_of(writer.connection(), "mfr_path", uuid)? else {
            continue;
        };
        if path.is_empty() {
            continue; // The root entry.
        }
        if !root.join(path.trim_start_matches('/')).exists() {
            orphans.push((uuid, path));
        }
    }

    // Step 3 — fingerprint phase. Hashes of disk files are computed lazily
    // and memoised across orphans. Per-orphan match lists are kept so the
    // similarity phase can extend them.
    let mut partial_cache: HashMap<String, String> = HashMap::new();
    let mut full_cache: HashMap<String, String> = HashMap::new();
    let mut claimed: HashSet<String> = HashSet::new();

    struct OrphanState {
        uuid: Uuid,
        stale_path: String,
        is_dir: bool,
        size: Option<i64>,
        matches: Vec<CandidateMatch>,
        moved: bool,
    }
    let mut states: Vec<OrphanState> = Vec::with_capacity(orphans.len());

    for (orphan, stale_path) in orphans {
        let is_dir = string_field(&writer, orphan, "mfr_type")?.as_deref() == Some("dir");
        let size = int_field(&writer, orphan, "mfr_size")?;
        let mut state =
            OrphanState { uuid: orphan, stale_path, is_dir, size, matches: Vec::new(), moved: false };

        // Directories have no fingerprint (matched only by path); orphans with
        // no stored size have no fingerprint either. Both can still be matched
        // by the similarity phase below.
        if is_dir || size.is_none() {
            states.push(state);
            continue;
        }
        let size = size.unwrap();
        let stored_partial = string_field(&writer, orphan, "mfr_partial_hash")?;
        let stored_full = string_field(&writer, orphan, "mfr_full_hash")?;

        let mut definitive: Option<String> = None;
        for (rel, meta) in &new_files {
            if claimed.contains(rel) || !meta.is_file() || meta.len() as i64 != size {
                continue;
            }
            let abs = root.join(rel.trim_start_matches('/'));
            match &stored_partial {
                None => state
                    .matches
                    .push(CandidateMatch { path: rel.clone(), fingerprint: "size", score: None }),
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
                            path: rel.clone(),
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
                claimed.insert(m.path.clone());
            }
        }
        states.push(state);
    }

    // Step 4 — similarity phase (v2): for each still-unmatched orphan and each
    // still-unmatched new path of the same kind, append score-based candidates.
    if let Some(threshold) = threshold {
        for state in states.iter_mut().filter(|s| !s.moved) {
            let orphan_sig = FileSig::from_path(&state.stale_path, state.size);
            for (rel, meta) in &new_files {
                if claimed.contains(rel) || meta.is_dir() != state.is_dir {
                    continue;
                }
                let new_size = meta.is_file().then_some(meta.len() as i64);
                let score = similarity_score(&orphan_sig, &FileSig::from_path(rel, new_size));
                if score >= threshold {
                    state
                        .matches
                        .push(CandidateMatch { path: rel.clone(), fingerprint: "similarity", score: Some(score) });
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
    new_files.sort_by_key(|(rel, _)| rel.matches('/').count());
    for (rel, _) in &new_files {
        if claimed.contains(rel) {
            continue;
        }
        if cache.resolve_path(writer.connection(), "mfr_path", rel)?.is_some() {
            continue; // Already created as a parent of an earlier path.
        }
        create_record_for(&mut writer, &mut cache, &root, rel, &[])?;
        result.created += 1;
    }

    // Step 5b — refresh phase (option): every file/directory still at its
    // recorded path gets its stat-derived `mfr_*` fields refreshed, catching
    // in-place edits made while the watcher was not running. Records just
    // created or moved above already hold current stat fields, so
    // `refresh_stat_fields` (which writes only changed fields) is a no-op for
    // them. Same behaviour as single-metarecord reconcile.
    if refresh {
        for (rel, _) in &fs_paths {
            if let Some(uuid) = cache.resolve_path(writer.connection(), "mfr_path", rel)? {
                refresh_stat_fields(&mut writer, &root, uuid, rel)?;
            }
        }
    }

    // Step 6 — MIME phase (spec-platform): every eligible file on disk now has
    // a record; fill in mfr_mime where it is still absent.
    if compute_mime {
        for rel in &disk_files {
            if let Some(uuid) = cache.resolve_path(writer.connection(), "mfr_path", rel)? {
                maybe_compute_mime(&mut writer, &root, uuid, rel)?;
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
    refresh: bool,
) -> Result<ReconcileResult, ApiError> {
    let mut conn = repo.conn.lock_recover();
    let mut cache = repo.lock_cache();
    let root = repo.config.root.clone();
    let db_id = repo.config.repo_uuid;

    if db::get_version(&conn, uuid)?.is_none() {
        return Err(ApiError::not_found(format!("Metarecord not found: {uuid}")));
    }
    let Some(base) = cache.path_of(&conn, "mfr_path", uuid)? else {
        return Err(ApiError::bad_request(format!(
            "entry {uuid} has no valid mfr_path (Nothing or unknown)"
        )));
    };

    let mut writer = Writer::begin(&mut conn, db_id, None)?;
    let mut result = ReconcileResult::default();

    let mut fs_paths: Vec<(String, Metadata)> = Vec::new();
    let abs_base = root.join(base.trim_start_matches('/'));
    if abs_base.exists() {
        let meta = std::fs::metadata(&abs_base).map_err(anyhow::Error::from)?;
        fs_paths.push((base.clone(), meta));
        walk(&mut writer, &mut cache, &root, &repo.internal_dir(), &base, &mut fs_paths)?;
    }

    fs_paths.sort_by_key(|(rel, _)| rel.matches('/').count());
    for (rel, _) in &fs_paths {
        // The subtree root itself was made eligible by the caller setting
        // mf_watch directly; descendants were eligibility-checked by walk().
        match cache.resolve_path(writer.connection(), "mfr_path", rel)? {
            Some(existing) => {
                if refresh {
                    refresh_stat_fields(&mut writer, &root, existing, rel)?;
                }
            }
            None => {
                create_record_for(&mut writer, &mut cache, &root, rel, &[])?;
                result.created += 1;
            }
        }
    }

    if compute_mime {
        for (rel, meta) in &fs_paths {
            if !meta.is_file() {
                continue;
            }
            if let Some(uuid) = cache.resolve_path(writer.connection(), "mfr_path", rel)? {
                maybe_compute_mime(&mut writer, &root, uuid, rel)?;
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
fn walk(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    internal_dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, Metadata)>,
) -> Result<()> {
    let abs = root.join(prefix.trim_start_matches('/'));
    let entries = match std::fs::read_dir(&abs) {
        Ok(entries) => entries,
        Err(_) => return Ok(()), // Not a directory or unreadable.
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            eprintln!("[reconcile] skipping non-UTF-8 name under {abs:?}");
            continue;
        };
        if entry.path() == internal_dir {
            continue;
        }
        let rel = format!("{prefix}/{name}");
        if !eligibility::is_eligible(writer.connection(), cache, &rel)? {
            continue;
        }
        let meta = entry.metadata()?;
        let is_dir = meta.is_dir();
        out.push((rel.clone(), meta));
        if is_dir {
            walk(writer, cache, root, internal_dir, &rel, out)?;
        }
    }
    Ok(())
}

/// Creates the metarecord for a new filesystem path (parents included).
pub(crate) fn create_record_for(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    rel: &str,
    extra_fields: &[Field],
) -> Result<Uuid> {
    let parent = ensure_parent_metarecords(writer, cache, root, rel, extra_fields)?;
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let mut fields = vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(parent), name: name.to_string() },
    )];
    fields.extend(fs_meta::stat_fields(&root.join(rel.trim_start_matches('/')))?);
    fields.extend(extra_fields.iter().cloned());
    let created = writer.create_metarecord(fields)?;
    cache.apply_insert("mfr_path", Some(parent), name, created.uuid);
    Ok(created.uuid)
}

/// Re-points an orphaned metarecord at its recovered location and refreshes its
/// stat fields.
fn apply_move(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    uuid: Uuid,
    rel: &str,
) -> Result<()> {
    let parent = ensure_parent_metarecords(writer, cache, root, rel, &[])?;
    let name = rel.rsplit('/').next().unwrap_or(rel);
    writer.set_field_as(
        OpType::FileMoved,
        uuid,
        "mfr_path",
        Value::TreeRef { parent: Some(parent), name: name.to_string() },
    )?;
    cache.apply_remove("mfr_path", uuid);
    cache.apply_insert("mfr_path", Some(parent), name, uuid);
    refresh_stat_fields(writer, root, uuid, rel)
}

/// Refreshes the stat-derived fields of an existing metarecord, writing only the
/// fields whose value actually changed (idempotent reconciles do not grow
/// the log).
fn refresh_stat_fields(writer: &mut Writer, root: &Path, uuid: Uuid, rel: &str) -> Result<()> {
    let Ok(stat) = fs_meta::stat_fields(&root.join(rel.trim_start_matches('/'))) else {
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
    Ok(db::get_field_rows_named(writer.connection(), uuid, name)?
        .into_iter()
        .find_map(|r| match r.value {
            Value::String(s) => Some(s),
            _ => None,
        }))
}

fn int_field(writer: &Writer, uuid: Uuid, name: &str) -> Result<Option<i64>> {
    Ok(db::get_field_rows_named(writer.connection(), uuid, name)?
        .into_iter()
        .find_map(|r| match r.value {
            Value::Int(n) => Some(n),
            _ => None,
        }))
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
fn maybe_compute_mime(writer: &mut Writer, root: &Path, uuid: Uuid, rel: &str) -> Result<()> {
    if !db::get_field_rows_named(writer.connection(), uuid, "mfr_mime")?.is_empty() {
        return Ok(());
    }
    let abs = root.join(rel.trim_start_matches('/'));
    if let Some(mime) = detect_mime(&abs) {
        writer.set_field_as(OpType::FileModified, uuid, "mfr_mime", Value::String(mime))?;
    }
    Ok(())
}

// ── File similarity (spec-file-tracking "File Similarity") ──────────────────────

/// Filename signature for similarity scoring.
struct FileSig {
    /// Basename without extension, lowercased.
    base: String,
    /// Extension without the dot, lowercased ("" when none).
    ext: String,
    size: Option<i64>,
    /// Directory components, lowercased.
    dirs: Vec<String>,
}

impl FileSig {
    fn from_path(rel: &str, size: Option<i64>) -> Self {
        let rel = rel.trim_start_matches('/');
        let (dir, name) = match rel.rfind('/') {
            Some(i) => (&rel[..i], &rel[i + 1..]),
            None => ("", rel),
        };
        // A leading dot is part of the name, not an extension separator.
        let (base, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
            _ => (name, ""),
        };
        let dirs =
            if dir.is_empty() { Vec::new() } else { dir.split('/').map(str::to_lowercase).collect() };
        FileSig { base: base.to_lowercase(), ext: ext.to_lowercase(), size, dirs }
    }
}

/// Character trigrams of a string (the whole string when shorter than 3 chars).
fn trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut set = HashSet::new();
    if chars.len() < 3 {
        if !s.is_empty() {
            set.insert(s.to_string());
        }
        return set;
    }
    for w in chars.windows(3) {
        set.insert(w.iter().collect());
    }
    set
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// Weighted four-signal similarity score in [0, 1]: trigram Jaccard of the
/// basename (0.5), extension match (0.2), size proximity (0.2), common
/// directory prefix (0.1).
fn similarity_score(a: &FileSig, b: &FileSig) -> f64 {
    let name_sim = jaccard(&trigrams(&a.base), &trigrams(&b.base));
    let ext_match = if a.ext == b.ext { 1.0 } else { 0.0 };
    let size_proximity = match (a.size, b.size) {
        (Some(x), Some(y)) => {
            let max = x.max(y);
            if max == 0 {
                1.0
            } else {
                (1.0 - (x - y).abs() as f64 / max as f64).max(0.0)
            }
        }
        _ => 0.0,
    };
    let max_depth = a.dirs.len().max(b.dirs.len());
    let path_sim = if max_depth == 0 {
        1.0
    } else {
        let common = a.dirs.iter().zip(&b.dirs).take_while(|(x, y)| x == y).count();
        common as f64 / max_depth as f64
    };
    0.5 * name_sim + 0.2 * ext_match + 0.2 * size_proximity + 0.1 * path_sim
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_files_score_one() {
        let a = FileSig::from_path("/music/jazz/song.mp3", Some(1000));
        assert!((similarity_score(&a, &a) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn renamed_same_dir_scores_above_threshold() {
        // A moved+modified file: same dir, same ext, similar name, close size.
        let a = FileSig::from_path("/music/jazz/old_song.mp3", Some(1000));
        let b = FileSig::from_path("/music/jazz/old_song_v2.mp3", Some(1100));
        assert!(similarity_score(&a, &b) >= 0.6, "score {}", similarity_score(&a, &b));
    }

    #[test]
    fn unrelated_files_score_low() {
        let a = FileSig::from_path("/music/jazz/song.mp3", Some(1000));
        let b = FileSig::from_path("/docs/report.pdf", Some(50));
        assert!(similarity_score(&a, &b) < 0.3, "score {}", similarity_score(&a, &b));
    }

    #[test]
    fn extension_mismatch_drops_the_ext_signal() {
        let a = FileSig::from_path("/a/name.mp3", Some(100));
        let b = FileSig::from_path("/a/name.wav", Some(100));
        // name_sim 1.0*0.5 + ext 0 + size 1.0*0.2 + path 1.0*0.1 = 0.8.
        assert!((similarity_score(&a, &b) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn unknown_size_zeroes_the_size_signal() {
        let a = FileSig::from_path("/a/name.mp3", None);
        let b = FileSig::from_path("/a/name.mp3", Some(100));
        // name 0.5 + ext 0.2 + size 0 + path 0.1 = 0.8.
        assert!((similarity_score(&a, &b) - 0.8).abs() < 1e-9);
    }
}
