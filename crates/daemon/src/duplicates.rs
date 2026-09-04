//! Duplicate detection (spec-duplicates.org): the scan that finds
//! byte-identical files and records each set as a `duplicate_group` metarecord
//! its members reference through `mfr_duplicate_group`.
//!
//! Four phases, each narrowing the candidate set before the next, more
//! expensive one runs: partition by size, partial hashes, full hashes (both
//! reusing what is already stored and still valid), then a prune of what no
//! longer holds. The scan reports only a summary — reading the result back is
//! an ordinary query over the fields it wrote, which is why it writes them.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use uuid::Uuid;

use metafolder_core::date::ms_from_systemtime;
use metafolder_core::metarecord::{Field, Value};
use metafolder_core::sync::MutexExt;

use crate::db::{self, StoredHashes};
use crate::error::ApiError;
use crate::fingerprint;
use crate::log::{OpType, Writer};
use crate::state::RepoState;
use crate::tasks::Reporter;

/// The `mf_schema` value marking a group metarecord.
pub const GROUP_SCHEMA: &str = "duplicate_group";

/// Records per revision when writing the hash cache. A transaction per file
/// would cost one `fsync` each (spec-event-log); one transaction for the whole
/// scan would throw away every hash computed so far when the scan is cancelled
/// — and the hash cache is precisely the work worth keeping.
const BATCH_RECORDS: usize = 500;

/// Progress is reported every this many items inside the scan loops.
const PROGRESS_STEP: usize = 64;

/// What the caller asks of a scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Files smaller than this are skipped. The default `1` excludes
    /// zero-length files, which are all identical to each other and free
    /// nothing when removed.
    pub min_size: i64,
    /// Ignore every stored hash and recompute — the escape hatch when the
    /// `stat` stamp cannot be trusted (spec-duplicates "The hash cache").
    pub rehash: bool,
    /// Restrict the scan to this metarecord's subtree.
    pub scope: Option<Uuid>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions { min_size: 1, rehash: false, scope: None }
    }
}

/// What a scan reports — a summary, never a listing.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct ScanResult {
    /// Groups holding at least two files after the scan.
    pub groups: usize,
    /// Files belonging to one of them.
    pub files: usize,
    /// Bytes freed by reducing every group to a single file, counting hard
    /// links to one inode once.
    pub reclaimable: i64,
    pub hashed_partial: usize,
    pub hashed_full: usize,
    /// Candidates the scan could not read (permissions, an offline volume).
    pub skipped: usize,
}

/// One file the scan is considering.
struct Candidate {
    uuid: Uuid,
    /// Repo-root-relative, leading `/`.
    rel: String,
    size: i64,
    /// `"<dev>:<ino>"` when the file has more than one name.
    inode: Option<String>,
    /// Filled by the partial phase, then the full phase.
    partial: Option<String>,
    full: Option<String>,
}

/// A scan with no progress reporting and no cancellation (tests, and any
/// synchronous caller).
pub fn scan(repo: &RepoState, opts: &ScanOptions) -> Result<ScanResult, ApiError> {
    scan_reported(repo, opts, &Reporter::silent())
}

/// The scan proper. Phases and their progress units are specified in
/// spec-tasks "Duplicate scan progress phases"; the `full` phase counts
/// *bytes*, because file sizes span orders of magnitude and a file count would
/// make the bar meaningless.
pub fn scan_reported(
    repo: &RepoState,
    opts: &ScanOptions,
    reporter: &Reporter,
) -> Result<ScanResult, ApiError> {
    let mut conn = repo.conn.lock_recover();
    let mut cache = repo.lock_cache();
    let root = repo.config.root.clone();
    let mut result = ScanResult::default();

    // ── Phase 1: partition by size ──────────────────────────────────────────
    let offline = crate::mount::offline(&conn, &mut cache, &root)?;
    let scope = match opts.scope {
        None => None,
        Some(uuid) => {
            let mut set: HashSet<Uuid> =
                cache.descendants(&conn, "mfr_path", uuid)?.into_iter().collect();
            set.insert(uuid);
            Some(set)
        }
    };
    let inodes: HashMap<Uuid, String> =
        db::string_field_owners(&conn, "mfr_inode")?.into_iter().collect();
    let files = db::tracked_files_with_size(&conn)?;

    let total = files.len() as u64;
    reporter.progress("size", Some(0), Some(total));
    let mut by_size: HashMap<i64, Vec<Candidate>> = HashMap::new();
    for (i, (uuid, size)) in files.into_iter().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("size", Some(i as u64), Some(total));
            if reporter.is_cancelled() {
                return Err(cancelled());
            }
        }
        if size < opts.min_size {
            continue;
        }
        if scope.as_ref().is_some_and(|s| !s.contains(&uuid)) {
            continue;
        }
        let Some(rel) = cache.path_of(&conn, "mfr_path", uuid)? else {
            continue;
        };
        // The filesystem root itself is a directory; an offline volume reads
        // back as an empty directory and must be left frozen, not scanned.
        if rel.is_empty() || offline.contains(&rel) {
            continue;
        }
        by_size.entry(size).or_default().push(Candidate {
            uuid,
            rel,
            size,
            inode: inodes.get(&uuid).cloned(),
            partial: None,
            full: None,
        });
    }
    // A size class of one cannot hold a duplicate: this is the cheap filter
    // that makes the rest tractable.
    by_size.retain(|_, members| members.len() > 1);

    // Largest first, so a cancelled scan still leaves the most valuable groups
    // recorded rather than nothing.
    let mut classes: Vec<(i64, Vec<Candidate>)> = by_size.into_iter().collect();
    classes.sort_by_key(|(size, _)| std::cmp::Reverse(*size));

    // ── Phase 2: partial hashes ─────────────────────────────────────────────
    let stored = db::hash_cache(&conn)?;
    let total = classes.iter().map(|(_, m)| m.len()).sum::<usize>() as u64;
    reporter.progress("partial", Some(0), Some(total));
    let mut pending: Vec<(Uuid, Field)> = Vec::new();
    let mut done = 0u64;
    for (_, members) in classes.iter_mut() {
        for member in members.iter_mut() {
            if done.is_multiple_of(PROGRESS_STEP as u64) {
                reporter.progress("partial", Some(done), Some(total));
                if reporter.is_cancelled() {
                    flush(&mut conn, &mut pending)?;
                    return Err(cancelled());
                }
            }
            done += 1;
            match hash_step(&root, member, &stored, opts, Fingerprint::Partial) {
                Ok(Step::Reused(hash)) => member.partial = Some(hash),
                Ok(Step::Computed(hash, stamp)) => {
                    result.hashed_partial += 1;
                    push_hash(&mut pending, member.uuid, "mfr_partial_hash", &hash, stamp);
                    member.partial = Some(hash);
                }
                Err(_) => result.skipped += 1,
            }
            if pending.len() >= BATCH_RECORDS {
                flush(&mut conn, &mut pending)?;
            }
        }
        // Within a size class, a partial hash occurring once eliminates its
        // file: nothing else can be identical to it.
        retain_shared(members, |m| m.partial.clone());
    }
    flush(&mut conn, &mut pending)?;
    classes.retain(|(_, members)| members.len() > 1);

    // ── Phase 3: full hashes, and the groups ────────────────────────────────
    // The byte total is known exactly before the phase starts, which is what
    // makes a remaining-time extrapolation honest.
    let total_bytes: i64 = classes.iter().map(|(size, m)| size * m.len() as i64).sum();
    reporter.progress("full", Some(0), Some(total_bytes.max(0) as u64));
    let mut hashed_bytes = 0i64;
    let mut existing = db::duplicate_groups(&conn)?;
    let mut grouped: HashSet<Uuid> = HashSet::new();
    let mut touched: HashSet<Uuid> = HashSet::new();

    for (size, members) in classes.iter_mut() {
        for member in members.iter_mut() {
            reporter.progress(
                "full",
                Some(hashed_bytes.max(0) as u64),
                Some(total_bytes.max(0) as u64),
            );
            if reporter.is_cancelled() {
                flush(&mut conn, &mut pending)?;
                return Err(cancelled());
            }
            match hash_step(&root, member, &stored, opts, Fingerprint::Full) {
                Ok(Step::Reused(hash)) => member.full = Some(hash),
                Ok(Step::Computed(hash, stamp)) => {
                    result.hashed_full += 1;
                    hashed_bytes += *size;
                    push_hash(&mut pending, member.uuid, "mfr_full_hash", &hash, stamp);
                    member.full = Some(hash);
                }
                Err(_) => result.skipped += 1,
            }
            if pending.len() >= BATCH_RECORDS {
                flush(&mut conn, &mut pending)?;
            }
        }
        flush(&mut conn, &mut pending)?;
        retain_shared(members, |m| m.full.clone());

        // This class's groups are written as it completes, not at the end.
        write_class_groups(
            &mut conn,
            *size,
            members,
            &mut existing,
            &mut grouped,
            &mut touched,
            &mut result,
        )?;
    }

    // ── Phase 4: prune what no longer holds ─────────────────────────────────
    // Only reached when the phases above completed: a cancelled scan leaves
    // stale groups for the next complete run rather than a half-pruned state.
    prune(&mut conn, &existing, &grouped, &touched, scope.as_ref(), reporter)?;

    Ok(result)
}

/// Which fingerprint step a [`hash_step`] call performs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fingerprint {
    Partial,
    Full,
}

/// The outcome of one fingerprint step.
enum Step {
    /// The stored hash was still valid for the file on disk.
    Reused(String),
    /// Computed afresh, with the `(mtime_ms, size)` it was computed under.
    Computed(String, (i64, i64)),
}

/// Computes one fingerprint, or reuses the stored one when its stamp still
/// matches the live `stat`.
///
/// The stamp (`mfr_hash_mtime` + `mfr_hash_size`) is what makes reuse safe in a
/// subtree whose watching is off — there, no `Modify` event ever invalidates
/// the hashes. Using mtime this way does not contradict spec-file-tracking's
/// "mtime is never used as a criterion": that rule governs *identity*, this is
/// *cache invalidation*, and it fails safe — a mismatch costs a recomputation,
/// never a wrong match.
fn hash_step(
    root: &Path,
    member: &Candidate,
    stored: &HashMap<Uuid, StoredHashes>,
    opts: &ScanOptions,
    which: Fingerprint,
) -> anyhow::Result<Step> {
    let abs = root.join(member.rel.trim_start_matches('/'));
    let meta = std::fs::symlink_metadata(&abs)?;
    let live = (meta.modified().map(ms_from_systemtime).unwrap_or_default(), meta.len() as i64);

    if !opts.rehash {
        if let Some(entry) = stored.get(&member.uuid) {
            let fresh = entry.stamp == Some(live);
            let hash = match which {
                Fingerprint::Partial => entry.partial.as_ref(),
                Fingerprint::Full => entry.full.as_ref(),
            };
            if let (true, Some(hash)) = (fresh, hash) {
                return Ok(Step::Reused(hash.clone()));
            }
        }
    }
    let hash = match which {
        Fingerprint::Partial => fingerprint::partial_hash(&abs)?,
        Fingerprint::Full => fingerprint::full_hash(&abs)?,
    };
    Ok(Step::Computed(hash, live))
}

/// Queues a computed hash and the stamp it was computed under. They are written
/// together so a stamp can never describe a hash that is not there.
fn push_hash(
    pending: &mut Vec<(Uuid, Field)>,
    uuid: Uuid,
    name: &str,
    hash: &str,
    (mtime, size): (i64, i64),
) {
    pending.push((uuid, Field::new(name, Value::String(hash.to_string()))));
    pending.push((uuid, Field::new("mfr_hash_mtime", Value::DateTime(mtime))));
    pending.push((uuid, Field::new("mfr_hash_size", Value::Int(size))));
}

/// Commits one batch of field writes as a single revision.
fn flush(
    conn: &mut rusqlite::Connection,
    pending: &mut Vec<(Uuid, Field)>,
) -> Result<(), ApiError> {
    if pending.is_empty() {
        return Ok(());
    }
    let mut writer = Writer::begin(conn, None).map_err(ApiError::from)?;
    for (uuid, field) in pending.drain(..) {
        writer
            .set_field_as(OpType::FileModified, uuid, &field.name, field.value)
            .map_err(ApiError::from)?;
    }
    writer.commit().map_err(ApiError::from)?;
    Ok(())
}

/// Keeps only the members whose key is shared with at least one other member,
/// dropping those whose key could not be computed at all.
fn retain_shared(members: &mut Vec<Candidate>, key: impl Fn(&Candidate) -> Option<String>) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for member in members.iter() {
        if let Some(k) = key(member) {
            *counts.entry(k).or_default() += 1;
        }
    }
    members.retain(|m| key(m).is_some_and(|k| counts.get(&k).copied().unwrap_or(0) > 1));
}

/// Bytes freed by reducing a group to a single file: its size times the number
/// of *distinct inodes* minus one. Members sharing an inode are one file under
/// several names, and removing a name frees nothing (spec-duplicates "Hard
/// links"). A member with no `mfr_inode` has a single name, so it counts as its
/// own inode.
fn reclaimable_of(size: i64, members: &[Candidate]) -> i64 {
    let mut distinct: HashSet<&str> = HashSet::new();
    let mut singles = 0i64;
    for member in members {
        match &member.inode {
            Some(inode) => {
                distinct.insert(inode.as_str());
            }
            None => singles += 1,
        }
    }
    size * (distinct.len() as i64 + singles - 1).max(0)
}

/// Writes one size class's groups, in one revision: find-or-create each group
/// by its `(size, hash)` identity, refresh its counters, and point every member
/// at it.
#[allow(clippy::too_many_arguments)]
fn write_class_groups(
    conn: &mut rusqlite::Connection,
    size: i64,
    members: &[Candidate],
    existing: &mut HashMap<(i64, String), Uuid>,
    grouped: &mut HashSet<Uuid>,
    touched: &mut HashSet<Uuid>,
    result: &mut ScanResult,
) -> Result<(), ApiError> {
    let mut by_hash: HashMap<String, Vec<&Candidate>> = HashMap::new();
    for member in members {
        if let Some(hash) = &member.full {
            by_hash.entry(hash.clone()).or_default().push(member);
        }
    }
    by_hash.retain(|_, m| m.len() > 1);
    if by_hash.is_empty() {
        return Ok(());
    }

    let mut writer = Writer::begin(conn, None).map_err(ApiError::from)?;
    for (hash, group_members) in by_hash {
        let owned: Vec<Candidate> = group_members
            .iter()
            .map(|m| Candidate {
                uuid: m.uuid,
                rel: m.rel.clone(),
                size: m.size,
                inode: m.inode.clone(),
                partial: None,
                full: None,
            })
            .collect();
        let reclaimable = reclaimable_of(size, &owned);
        let count = group_members.len() as i64;

        // Identity is the pair (size, hash), not the hash alone: two different
        // size classes could in principle produce the same hash, and the
        // find-or-create has to stay well defined.
        let group = match existing.get(&(size, hash.clone())) {
            Some(uuid) => *uuid,
            None => {
                let created = writer
                    .create_metarecord(vec![
                        Field::new("mf_schema", Value::String(GROUP_SCHEMA.to_string())),
                        Field::new("mfr_content_hash", Value::String(hash.clone())),
                        Field::new("mfr_content_size", Value::Int(size)),
                    ])
                    .map_err(ApiError::from)?
                    .uuid;
                existing.insert((size, hash.clone()), created);
                created
            }
        };
        set_if_changed(&mut writer, group, "mfr_duplicate_count", Value::Int(count))?;
        set_if_changed(&mut writer, group, "mfr_duplicate_reclaimable", Value::Int(reclaimable))?;
        touched.insert(group);

        for member in group_members {
            set_if_changed(&mut writer, member.uuid, "mfr_duplicate_group", Value::Ref(group))?;
            grouped.insert(member.uuid);
        }
        result.groups += 1;
        result.files += count as usize;
        result.reclaimable += reclaimable;
    }
    writer.commit().map_err(ApiError::from)?;
    Ok(())
}

/// Writes a field only when its stored value differs, so a re-scan of an
/// unchanged repository produces no operation — and `Writer::commit` then drops
/// the empty revision entirely.
fn set_if_changed(
    writer: &mut Writer,
    uuid: Uuid,
    name: &str,
    value: Value,
) -> Result<(), ApiError> {
    let current =
        db::get_field_rows_named(writer.connection(), uuid, name).map_err(ApiError::from)?;
    if current.len() == 1 && current[0].value == value {
        return Ok(());
    }
    writer.set_field_as(OpType::FileModified, uuid, name, value).map_err(ApiError::from)?;
    Ok(())
}

/// Removes what the scan disproved: the group link of a record the scan did not
/// place in a group, and every group left with fewer than two members.
fn prune(
    conn: &mut rusqlite::Connection,
    existing: &HashMap<(i64, String), Uuid>,
    grouped: &HashSet<Uuid>,
    touched: &HashSet<Uuid>,
    scope: Option<&HashSet<Uuid>>,
    reporter: &Reporter,
) -> Result<(), ApiError> {
    let total = existing.len() as u64;
    reporter.progress("prune", Some(0), Some(total));
    let mut writer = Writer::begin(conn, None).map_err(ApiError::from)?;

    // Driven by the records that *currently carry a link* — one query, and it
    // catches every way of ceasing to be a duplicate at once: the twin changed,
    // the file grew out of its size class, it fell under `min_size`, it stopped
    // being a file. This is what keeps "`mfr_duplicate_group` present ⟹ it had
    // a twin at the last scan" true. A scoped scan touches only its own subtree.
    let linked = db::metarecords_with_field(writer.connection(), "mfr_duplicate_group")
        .map_err(ApiError::from)?;
    for uuid in linked {
        if grouped.contains(&uuid) || scope.is_some_and(|s| !s.contains(&uuid)) {
            continue;
        }
        writer
            .clear_field_as(OpType::FileModified, uuid, "mfr_duplicate_group")
            .map_err(ApiError::from)?;
    }

    for (i, group) in existing.values().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("prune", Some(i as u64), Some(total));
        }
        if touched.contains(group) {
            continue;
        }
        let members = db::ref_field_owners(writer.connection(), "mfr_duplicate_group", *group)
            .map_err(ApiError::from)?;
        if members.len() > 1 {
            continue;
        }
        for member in members {
            writer
                .clear_field_as(OpType::FileModified, member, "mfr_duplicate_group")
                .map_err(ApiError::from)?;
        }
        writer.delete_metarecord(*group).map_err(ApiError::from)?;
    }
    writer.commit().map_err(ApiError::from)?;
    Ok(())
}

fn cancelled() -> ApiError {
    ApiError::conflict("duplicate scan cancelled")
}
