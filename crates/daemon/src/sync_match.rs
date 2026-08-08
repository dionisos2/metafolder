//! Cross-repo match suggestions for `POST /sync/:a/:b/candidates` (spec-sync
//! "candidates"): for each unlinked source metarecord, propose the best unlinked
//! target metarecord to link it to. Matching uses **stored metadata only** — no
//! file is opened and nothing is hashed here.
//!
//! Three signals, in decreasing confidence:
//! - `full` — identical stored `mfr_full_hash`.
//! - `path` — identical reconstructed `mfr_path`.
//! - `similar` — best filename [`similarity_score`] ≥ the request `threshold`
//!   (only when a threshold is given). MinHash content sketches are deferred.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde_json::{json, Value as Json};
use uuid::Uuid;

use metafolder_core::metarecord::Value;
use metafolder_core::sync::MutexExt;

use crate::db;
use crate::error::ApiError;
use crate::routes::hex;
use crate::similarity::{similarity_score, FileSig};
use crate::state::RepoState;
use crate::tree_cache::TreeCache;

/// A target-repo metarecord's precomputed matching profile (for the similarity
/// fallback; exact hash/path matches are served from the inline indexes).
struct Profile {
    uuid: Uuid,
    path: Option<String>,
    size: Option<i64>,
}

/// Computes link candidates from `src` into `tgt`. `linked_src`/`linked_tgt` are
/// the records already linked in this pair (on the source/target side): both
/// endpoints of a candidate must be unlinked. `records`, when given, restricts
/// the source set (else all tracked source records). `threshold` enables the
/// similarity fallback.
pub fn candidates(
    src: &RepoState,
    tgt: &RepoState,
    linked_src: &HashSet<Uuid>,
    linked_tgt: &HashSet<Uuid>,
    records: Option<&[Uuid]>,
    threshold: Option<f64>,
) -> Result<Vec<Json>, ApiError> {
    // Acquire every lock of the canonically-smaller repo before the larger
    // repo's, so two concurrent candidate calls on the same pair (opposite
    // source directions) can never deadlock.
    let src_first = src.config.repo_uuid.as_bytes() < tgt.config.repo_uuid.as_bytes();
    let (lo, hi) = if src_first { (src, tgt) } else { (tgt, src) };
    let lo_conn = lo.conn.lock_recover();
    let mut lo_cache = lo.lock_cache();
    let hi_conn = hi.conn.lock_recover();
    let mut hi_cache = hi.lock_cache();
    let (src_conn, src_cache, tgt_conn, tgt_cache): (
        &Connection,
        &mut TreeCache,
        &Connection,
        &mut TreeCache,
    ) = if src_first {
        (&lo_conn, &mut lo_cache, &hi_conn, &mut hi_cache)
    } else {
        (&hi_conn, &mut hi_cache, &lo_conn, &mut lo_cache)
    };

    // Target index: unlinked tracked target records, by hash and by path.
    let mut profiles: Vec<Profile> = Vec::new();
    let mut by_hash: HashMap<String, Uuid> = HashMap::new();
    let mut by_path: HashMap<String, Uuid> = HashMap::new();
    for uuid in db::all_tracked_metarecords(tgt_conn)? {
        if linked_tgt.contains(&uuid) || is_forest_root(tgt_conn, uuid)? {
            continue;
        }
        let full_hash = first_string(tgt_conn, uuid, "mfr_full_hash")?;
        let size = first_int(tgt_conn, uuid, "mfr_size")?;
        let path = tgt_cache.path_of(tgt_conn, "mfr_path", uuid)?;
        if let Some(h) = &full_hash {
            by_hash.entry(h.clone()).or_insert(uuid);
        }
        if let Some(p) = &path {
            by_path.entry(p.clone()).or_insert(uuid);
        }
        profiles.push(Profile { uuid, path, size });
    }

    let source: Vec<Uuid> = match records {
        Some(rs) => rs.to_vec(),
        None => db::all_tracked_metarecords(src_conn)?,
    };

    let mut out = Vec::new();
    for uuid in source {
        if linked_src.contains(&uuid) || is_forest_root(src_conn, uuid)? {
            continue;
        }
        let full_hash = first_string(src_conn, uuid, "mfr_full_hash")?;
        let path = src_cache.path_of(src_conn, "mfr_path", uuid)?;
        let size = first_int(src_conn, uuid, "mfr_size")?;

        if let Some(t) = full_hash.as_deref().and_then(|h| by_hash.get(h)).copied() {
            out.push(candidate(uuid, t, "full", 1.0));
            continue;
        }
        if let Some(t) = path.as_deref().and_then(|p| by_path.get(p)).copied() {
            out.push(candidate(uuid, t, "path", 1.0));
            continue;
        }
        if let Some(th) = threshold {
            let sig = FileSig::from_path(path.as_deref().unwrap_or(""), size);
            let mut best: Option<(Uuid, f64)> = None;
            for p in &profiles {
                // A hash match would already have been taken above; here we only
                // reach records with no exact match, so score by filename.
                let score = similarity_score(&sig, &FileSig::from_path(p.path.as_deref().unwrap_or(""), p.size));
                if score >= th && best.is_none_or(|(_, b)| score > b) {
                    best = Some((p.uuid, score));
                }
            }
            if let Some((t, score)) = best {
                out.push(candidate(uuid, t, "similar", score));
            }
        }
    }
    Ok(out)
}

fn candidate(source: Uuid, target: Uuid, kind: &str, score: f64) -> Json {
    json!({ "source": hex(source), "target": hex(target), "kind": kind, "score": score })
}

/// Whether a metarecord is the repository's `mfr_path` forest root — the empty
/// -named anchor at the top of the tree (`find_tree_child(.., None, "")`). Real
/// files are always children of it (a non-empty name), so the root is the only
/// such record; it is structural, not content, and never a sync candidate.
fn is_forest_root(conn: &Connection, uuid: Uuid) -> Result<bool, ApiError> {
    Ok(db::get_field_rows_named(conn, uuid, "mfr_path")?
        .into_iter()
        .any(|r| matches!(r.value, Value::TreeRef { ref name, .. } if name.is_empty())))
}

/// First `String` value of a field on a metarecord.
fn first_string(conn: &Connection, uuid: Uuid, name: &str) -> Result<Option<String>, ApiError> {
    Ok(db::get_field_rows_named(conn, uuid, name)?
        .into_iter()
        .find_map(|r| match r.value {
            Value::String(s) => Some(s),
            _ => None,
        }))
}

/// First `Int` value of a field on a metarecord.
fn first_int(conn: &Connection, uuid: Uuid, name: &str) -> Result<Option<i64>, ApiError> {
    Ok(db::get_field_rows_named(conn, uuid, name)?
        .into_iter()
        .find_map(|r| match r.value {
            Value::Int(n) => Some(n),
            _ => None,
        }))
}
