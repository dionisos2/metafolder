//! Reconcile (spec-file-tracking): synchronises the database with the
//! filesystem on demand. The fingerprint phase recovers moved files; new
//! files get records; orphaned records keep their stale path (reconcile
//! never writes Nothing).

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use uuid::Uuid;

use metafolder_core::record::{Field, Value};

use crate::db;
use crate::eligibility;
use crate::error::ApiError;
use crate::executor::ensure_parent_records;
use crate::fingerprint;
use crate::fs_meta;
use crate::log::{OpType, Writer};
use crate::state::RepoState;
use crate::tree_cache::TreeCache;

#[derive(Debug, Serialize)]
pub struct CandidateMatch {
    pub path: String,
    /// `"partial_hash"` (strong) or `"size"` (weak).
    pub fingerprint: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Candidate {
    #[serde(with = "metafolder_core::record::hex_uuid")]
    pub record_uuid: Uuid,
    pub stale_path: String,
    pub matches: Vec<CandidateMatch>,
}

#[derive(Debug, Default, Serialize)]
pub struct ReconcileResult {
    pub created: usize,
    pub moved: usize,
    pub candidates: Vec<Candidate>,
}

/// Full reconcile: walk the repository root and synchronise the database.
/// Everything runs in a single transaction (one revision).
pub fn reconcile(repo: &RepoState) -> Result<ReconcileResult, ApiError> {
    let mut conn = repo.conn.lock().unwrap();
    let mut cache = repo.cache.lock().unwrap();
    let root = repo.config.root.clone();
    let db_id = repo.config.repo_uuid;
    let mut writer = Writer::begin(&mut conn, db_id, None)?;
    let mut result = ReconcileResult::default();

    // Step 2 — walk the filesystem (eligibility-pruned).
    let internal_dir = repo.internal_dir();
    let mut fs_paths: Vec<(String, Metadata)> = Vec::new();
    walk(&mut writer, &mut cache, &root, &internal_dir, "", &mut fs_paths)?;

    // New files: paths with no record at that tree position.
    let mut new_files: Vec<(String, Metadata)> = Vec::new();
    for (rel, meta) in fs_paths {
        if cache.resolve_path(writer.connection(), "mfr_path", &rel)?.is_none() {
            new_files.push((rel, meta));
        }
    }

    // Step 1 — orphaned records: tree position no longer present on disk.
    // (Checked against the disk directly, so that files that merely became
    // ineligible are not mistaken for orphans.)
    let mut orphans: Vec<(Uuid, String)> = Vec::new();
    for uuid in db::all_tracked_records(writer.connection(), db_id)? {
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
    // and memoised across orphans.
    let mut partial_cache: HashMap<String, String> = HashMap::new();
    let mut full_cache: HashMap<String, String> = HashMap::new();
    let mut claimed: HashSet<String> = HashSet::new();

    for (orphan, stale_path) in orphans {
        // Directories have no fingerprint (matched only by path).
        if string_field(&writer, orphan, "mfr_type")?.as_deref() == Some("dir") {
            continue;
        }
        let Some(size) = int_field(&writer, orphan, "mfr_size")? else {
            continue; // No fingerprint available at all: skipped.
        };
        let stored_partial = string_field(&writer, orphan, "mfr_partial_hash")?;
        let stored_full = string_field(&writer, orphan, "mfr_full_hash")?;

        let mut matches: Vec<CandidateMatch> = Vec::new();
        let mut definitive: Option<String> = None;
        for (rel, meta) in &new_files {
            if claimed.contains(rel) || !meta.is_file() || meta.len() as i64 != size {
                continue;
            }
            let abs = root.join(rel.trim_start_matches('/'));
            match &stored_partial {
                None => matches.push(CandidateMatch { path: rel.clone(), fingerprint: "size" }),
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
                        None => matches
                            .push(CandidateMatch { path: rel.clone(), fingerprint: "partial_hash" }),
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
            result.moved += 1;
        } else if !matches.is_empty() {
            // Candidate files wait for explicit confirmation: they are
            // neither auto-matched nor auto-created.
            for m in &matches {
                claimed.insert(m.path.clone());
            }
            result.candidates.push(Candidate { record_uuid: orphan, stale_path, matches });
        }
    }

    // Step 5 — create records for the remaining new files, parents first.
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

    writer.commit()?;
    Ok(result)
}

/// Single-record reconcile: same semantics scoped to the subtree rooted at
/// the given record, without the fingerprint phase. Existing records get
/// their `mfr_*` stat fields refreshed.
pub fn reconcile_record(repo: &RepoState, uuid: Uuid) -> Result<ReconcileResult, ApiError> {
    let mut conn = repo.conn.lock().unwrap();
    let mut cache = repo.cache.lock().unwrap();
    let root = repo.config.root.clone();
    let db_id = repo.config.repo_uuid;

    if db::get_version(&conn, uuid)?.is_none() {
        return Err(ApiError::not_found(format!("Record not found: {uuid}")));
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
            Some(existing) => refresh_stat_fields(&mut writer, &root, existing, rel)?,
            None => {
                create_record_for(&mut writer, &mut cache, &root, rel, &[])?;
                result.created += 1;
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

/// Creates the record for a new filesystem path (parents included).
pub(crate) fn create_record_for(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    rel: &str,
    extra_fields: &[Field],
) -> Result<Uuid> {
    let parent = ensure_parent_records(writer, cache, root, rel, extra_fields)?;
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let mut fields = vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(parent), name: name.to_string() },
    )];
    fields.extend(fs_meta::stat_fields(&root.join(rel.trim_start_matches('/')))?);
    fields.extend(extra_fields.iter().cloned());
    let created = writer.create_record(fields)?;
    cache.apply_insert("mfr_path", Some(parent), name, created.uuid);
    Ok(created.uuid)
}

/// Re-points an orphaned record at its recovered location and refreshes its
/// stat fields.
fn apply_move(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    uuid: Uuid,
    rel: &str,
) -> Result<()> {
    let parent = ensure_parent_records(writer, cache, root, rel, &[])?;
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

/// Refreshes the stat-derived fields of an existing record, writing only the
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
