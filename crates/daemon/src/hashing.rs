//! Content hashes computed on demand (doc "Content hashes kept fresh"): the
//! partial and full hash of a set of files, with the `stat` stamp they were
//! taken under, reusing a stored hash its stamp still vouches for (doc "The
//! duplicate hash cache").

use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;
use uuid::Uuid;

use metafolder_core::date::ms_from_systemtime;
use metafolder_core::metarecord::Value;
use metafolder_core::query::Query;
use metafolder_core::sync::MutexExt;

use crate::error::ApiError;
use crate::fingerprint;
use crate::log::{OpType, WriteContext, Writer};
use crate::relpath::RelPath;
use crate::rows::StoredHashes;
use crate::state::RepoState;
use crate::store::Rows;
use crate::tasks::Reporter;

/// How long computed hashes accumulate before they are written: each write is
/// an `fsync`, and what an error loses is bounded by it (the same trade as the
/// duplicate scan's, doc "The duplicate scan").
const COMMIT_EVERY: Duration = Duration::from_secs(20);

/// The longest one write holds the store before committing and giving it back.
const HOLD: Duration = Duration::from_millis(250);

/// What the caller asks of `mf hash`.
#[derive(Debug, Clone, Default)]
pub struct HashOptions {
    /// Ignore every stored hash and recompute.
    pub rehash: bool,
}

/// What `mf hash` reports.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct HashResult {
    /// Files whose hashes were computed.
    pub hashed: usize,
    /// Files whose stored hashes were still valid.
    pub reused: usize,
    /// Files that could not be read, or changed while they were.
    pub skipped: usize,
}

/// The `(mtime_ms, size)` stamp of the entry at `abs`, as `stat` gives it now.
pub fn live_stamp(abs: &Path) -> std::io::Result<(i64, i64)> {
    let meta = std::fs::symlink_metadata(abs)?;
    Ok((meta.modified().map(ms_from_systemtime).unwrap_or_default(), meta.len() as i64))
}

/// Both hashes of one file, and the stamp the file had before *and* after they
/// were read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    pub partial: String,
    pub full: String,
    pub stamp: (i64, i64),
}

/// What hashing one file came to.
#[derive(Debug)]
pub enum Outcome {
    /// The stored hashes are valid for the file as it is.
    Reused,
    Computed(Hashes),
    /// Unreadable, gone, or changed while being read.
    Skipped,
}

/// Hashes the file at `abs`, or reuses `stored` when its stamp matches the
/// live `stat` (and `rehash` is off). A file whose stamp moved while it was
/// read is skipped: its hashes would describe no state it was ever in.
pub fn hash_file(abs: &Path, stored: Option<&StoredHashes>, rehash: bool) -> Outcome {
    let Ok(before) = live_stamp(abs) else {
        return Outcome::Skipped;
    };
    if !rehash {
        if let Some(s) = stored {
            if s.stamp == Some(before) && s.partial.is_some() && s.full.is_some() {
                return Outcome::Reused;
            }
        }
    }
    let (Ok(partial), Ok(full)) = (fingerprint::partial_hash(abs), fingerprint::full_hash(abs))
    else {
        return Outcome::Skipped;
    };
    match live_stamp(abs) {
        Ok(after) if after == before => Outcome::Computed(Hashes { partial, full, stamp: before }),
        _ => Outcome::Skipped,
    }
}

/// Writes a file's hashes and their stamp — only the fields that differ, so
/// hashing an unchanged file again moves no version. A record gone since is
/// left alone.
pub fn write_hashes(writer: &mut Writer, uuid: Uuid, h: &Hashes) -> anyhow::Result<()> {
    if Rows::version(writer.store(), uuid)?.is_none() {
        return Ok(());
    }
    let (mtime, size) = h.stamp;
    for (field, value) in [
        ("mfr_partial_hash", Value::String(h.partial.clone())),
        ("mfr_full_hash", Value::String(h.full.clone())),
        ("mfr_hash_mtime", Value::DateTime(mtime)),
        ("mfr_hash_size", Value::Int(size)),
    ] {
        let current = Rows::rows_named(writer.store(), uuid, field)?;
        if current.len() == 1 && current[0].value == value {
            continue;
        }
        writer.set_field_as(OpType::FileModified, uuid, field, value)?;
    }
    Ok(())
}

/// [`hash_files_reported`] over every file `query` selects, with no progress
/// reporting (tests, and any synchronous caller).
pub fn hash_query(
    repo: &RepoState,
    query: &Query,
    opts: &HashOptions,
) -> Result<HashResult, ApiError> {
    hash_query_reported(repo, query, opts, &Reporter::silent())
}

/// `mf hash QUERY`: the partial and full hash of every *file* the query
/// selects — a directory or a symlink has none —, read with the store free,
/// written by windows. Progress counts bytes; a cancel writes what was computed
/// first.
pub fn hash_query_reported(
    repo: &RepoState,
    query: &Query,
    opts: &HashOptions,
    reporter: &Reporter,
) -> Result<HashResult, ApiError> {
    let root = repo.config.root.clone();
    let (files, stored) = {
        let conn = metafolder_core::slowlog::timed("wait:conn", || repo.conn.lock_recover());
        let cache = repo.tree();
        let uuids = crate::routes::resolve_query_uuids(&conn, &cache, query)?;
        let offline = crate::mount::offline(&conn, &cache, &root)?;
        let mut files: Vec<(Uuid, RelPath, i64)> = Vec::new();
        for uuid in uuids {
            let kind = Rows::string_fields(&*conn, uuid, "mfr_type")?;
            if kind.first().map(String::as_str) != Some("file") {
                continue;
            }
            let Some(rel) = cache.rel_path_of(&conn, uuid)? else {
                continue;
            };
            if offline.contains(&rel.display()) {
                continue;
            }
            let size = Rows::int_field(&*conn, uuid, "mfr_size")?.unwrap_or(0);
            files.push((uuid, rel, size));
        }
        let stored = crate::store::Questions::hash_cache(&*conn)?;
        (files, stored)
    };

    let total: i64 = files.iter().map(|(_, _, size)| *size).sum();
    let total = total.max(0) as u64;
    let mut done = 0u64;
    let mut result = HashResult::default();
    let mut pending: Vec<(Uuid, Hashes)> = Vec::new();
    let mut context = crate::log::current_context().named("hash");
    let mut opened = Instant::now();
    for (uuid, rel, size) in files {
        reporter.progress("hash", Some(done), Some(total));
        if reporter.is_cancelled() {
            flush(repo, &mut pending, &mut context)?;
            return Err(ApiError::conflict("hashing cancelled"));
        }
        match hash_file(&rel.to_abs(&root), stored.get(&uuid), opts.rehash) {
            Outcome::Reused => result.reused += 1,
            Outcome::Computed(h) => {
                result.hashed += 1;
                pending.push((uuid, h));
            }
            Outcome::Skipped => result.skipped += 1,
        }
        done += size.max(0) as u64;
        if opened.elapsed() >= COMMIT_EVERY {
            flush(repo, &mut pending, &mut context)?;
            opened = Instant::now();
        }
    }
    flush(repo, &mut pending, &mut context)?;
    reporter.progress("hash", Some(total), Some(total));
    Ok(result)
}

/// Writes what is pending, in takes of at most [`HOLD`], committing between
/// them so a waiting request gets the store.
fn flush(
    repo: &RepoState,
    pending: &mut Vec<(Uuid, Hashes)>,
    context: &mut WriteContext,
) -> Result<(), ApiError> {
    let mut items = std::mem::take(pending);
    // In record order: a commit then dirties neighbouring pages.
    items.sort_by_key(|(uuid, _)| *uuid);
    let mut items = items.into_iter().peekable();
    while items.peek().is_some() {
        let mut conn = metafolder_core::slowlog::timed("wait:conn", || repo.conn.lock_recover());
        let held = Instant::now();
        let mut writer = repo.writer(&mut *conn, context.clone())?;
        while let Some((uuid, h)) = items.next() {
            write_hashes(&mut writer, uuid, &h)?;
            if held.elapsed() >= HOLD {
                break;
            }
        }
        if writer.op_count() > 0 {
            *context = WriteContext::In(writer.label_id());
        }
        metafolder_core::slowlog::timed("commit", || writer.commit())?;
        drop(conn);
        if items.peek().is_some() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    Ok(())
}
