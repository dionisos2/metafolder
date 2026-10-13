//! Duplicate detection (doc "Duplicates"): the scan that finds
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
use std::time::Instant;

use serde::Serialize;
use uuid::Uuid;

use metafolder_core::date::ms_from_systemtime;
use metafolder_core::metarecord::{Field, Value};
use metafolder_core::sync::MutexExt;

use crate::error::ApiError;
use crate::fingerprint;
use crate::log::{OpType, Writer};
use crate::rows::{self, StoredHashes};
use crate::state::RepoState;
use crate::tasks::Reporter;

/// The `mf_schema` value marking a group metarecord.
pub const GROUP_SCHEMA: &str = "duplicate_group";

/// The field a member points at its group with. One spelling, because leaving a
/// group is never a bare field clear: see [`leave_group`].
pub const GROUP_FIELD: &str = "mfr_duplicate_group";

/// How long the scan's writes accumulate before it takes the store to write
/// them. The hashing runs with the store free (doc "The duplicate scan"), so
/// the window no longer decides how long others wait; it bounds how many
/// commits a scan makes — every commit is an `fsync` (doc "Writes and
/// atomicity"), and committing per batch of files and per size class spent 17
/// of 31 s committing on 50 000 files — and what an error or a crash can lose.
/// A cancel loses nothing: it writes what is pending first. One write for the
/// whole scan would throw every hash computed so far away on an error, and the
/// hash cache is precisely the work worth keeping.
const COMMIT_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

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
    /// `stat` stamp cannot be trusted (doc "The duplicate hash cache").
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
    /// The `(mtime_ms, size)` the file had when its full hash was taken: what
    /// it must still have for the hash to place it in a group.
    stamp: Option<(i64, i64)>,
}

/// A scan with no progress reporting and no cancellation (tests, and any
/// synchronous caller).
pub fn scan(repo: &RepoState, opts: &ScanOptions) -> Result<ScanResult, ApiError> {
    scan_reported(repo, opts, &Reporter::silent())
}

/// The scan proper. Phases and their progress units are specified in
/// doc "The duplicate scan"; the `full` phase counts
/// *bytes*, because file sizes span orders of magnitude and a file count would
/// make the bar meaningless.
pub fn scan_reported(
    repo: &RepoState,
    opts: &ScanOptions,
    reporter: &Reporter,
) -> Result<ScanResult, ApiError> {
    let conn = metafolder_core::slowlog::timed("wait:conn", || repo.conn.lock_recover());
    let cache = repo.tree();
    let root = repo.config.root.clone();
    let mut result = ScanResult::default();

    // ── Phase 1: partition by size ──────────────────────────────────────────
    let offline = crate::mount::offline(&conn, &cache, &root)?;
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
        crate::store::Questions::string_owners(&conn, "mfr_inode")?.into_iter().collect();
    let files = crate::store::Questions::tracked_files_with_size(&conn)?;

    // Partition on `(uuid, size)` alone first. A size class of one cannot hold
    // a duplicate, and that is by far the biggest cut of the scan — so nothing
    // heavier than an integer is built until it has been made: resolving every
    // eligible file's path here would construct a string per file and throw
    // most of them away a moment later.
    let total = files.len() as u64;
    reporter.progress("size", Some(0), Some(total));
    let mut by_size: HashMap<i64, Vec<Uuid>> = HashMap::new();
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
        by_size.entry(size).or_default().push(uuid);
    }
    by_size.retain(|_, members| members.len() > 1);

    // Only now, for the survivors, the per-file work: a path (and with it the
    // offline-volume guard, which needs one).
    let mut classes: Vec<(i64, Vec<Candidate>)> = Vec::with_capacity(by_size.len());
    for (size, uuids) in by_size {
        let mut members = Vec::with_capacity(uuids.len());
        for uuid in uuids {
            let Some(rel) = cache.path_of(&conn, "mfr_path", uuid)? else {
                continue;
            };
            // The filesystem root itself is a directory; an offline volume
            // reads back as an empty directory and must be left frozen, not
            // scanned.
            if rel.is_empty() || offline.contains(&rel) {
                continue;
            }
            members.push(Candidate {
                uuid,
                rel,
                size,
                inode: inodes.get(&uuid).cloned(),
                partial: None,
                full: None,
                stamp: None,
            });
        }
        // A class can fall below two members here — an unresolvable path, an
        // offline volume. Dropping it is an economy rather than a correction:
        // the partial-hash phase eliminates a lone member anyway, but only
        // after opening the file and reading 8 KiB of it for nothing.
        if members.len() > 1 {
            classes.push((size, members));
        }
    }

    // Largest first, so a cancelled scan still leaves the most valuable groups
    // recorded rather than nothing.
    classes.sort_by_key(|(size, _)| std::cmp::Reverse(*size));

    // ── Phase 2: partial hashes ─────────────────────────────────────────────
    let stored = crate::store::Questions::hash_cache(&conn)?;
    // The rest of the scan reads files, not the store: it runs with the store
    // free, and takes it back only to write what it found (`flush`), every
    // `COMMIT_EVERY` and at the end.
    drop(conn);
    let mut writes = Writes {
        repo,
        // One label for the whole scan, its writes being one piece of work: the
        // first that writes opens it, and the next ones write inside it.
        context: crate::log::current_context().named("duplicate scan"),
        hashes: Vec::new(),
        classes: Vec::new(),
        grouped: HashSet::new(),
        touched: HashSet::new(),
        opened: Instant::now(),
    };
    let total = classes.iter().map(|(_, m)| m.len()).sum::<usize>() as u64;
    reporter.progress("partial", Some(0), Some(total));
    let mut done = 0u64;
    for (_, members) in classes.iter_mut() {
        for member in members.iter_mut() {
            if done.is_multiple_of(PROGRESS_STEP as u64) {
                reporter.progress("partial", Some(done), Some(total));
                if reporter.is_cancelled() {
                    writes.flush(&mut result, None)?;
                    return Err(cancelled());
                }
            }
            done += 1;
            match hash_step(&root, member, &stored, opts, Fingerprint::Partial) {
                Ok(Step::Reused(hash, _)) => member.partial = Some(hash),
                Ok(Step::Computed(hash, stamp)) => {
                    result.hashed_partial += 1;
                    writes.hashes.push((member.uuid, "mfr_partial_hash", hash.clone(), stamp));
                    member.partial = Some(hash);
                }
                Err(_) => result.skipped += 1,
            }
            writes.flush_when_due(&mut result)?;
        }
        // Within a size class, a partial hash occurring once eliminates its
        // file: nothing else can be identical to it.
        retain_shared(members, |m| m.partial.clone());
    }
    classes.retain(|(_, members)| members.len() > 1);

    // ── Phase 3: full hashes, and the groups ────────────────────────────────
    // The byte total is known exactly before the phase starts, which is what
    // makes a remaining-time extrapolation honest.
    let total_bytes: i64 = classes.iter().map(|(size, m)| size * m.len() as i64).sum();
    reporter.progress("full", Some(0), Some(total_bytes.max(0) as u64));
    let mut hashed_bytes = 0i64;

    for (size, mut members) in classes {
        for member in members.iter_mut() {
            reporter.progress(
                "full",
                Some(hashed_bytes.max(0) as u64),
                Some(total_bytes.max(0) as u64),
            );
            if reporter.is_cancelled() {
                writes.flush(&mut result, None)?;
                return Err(cancelled());
            }
            match hash_step(&root, member, &stored, opts, Fingerprint::Full) {
                Ok(Step::Reused(hash, stamp)) => {
                    member.full = Some(hash);
                    member.stamp = Some(stamp);
                }
                Ok(Step::Computed(hash, stamp)) => {
                    result.hashed_full += 1;
                    hashed_bytes += size;
                    writes.hashes.push((member.uuid, "mfr_full_hash", hash.clone(), stamp));
                    member.full = Some(hash);
                    member.stamp = Some(stamp);
                }
                Err(_) => result.skipped += 1,
            }
            writes.flush_when_due(&mut result)?;
        }
        retain_shared(&mut members, |m| m.full.clone());
        // This class's groups go out with the next write, not at the end: a
        // cancelled scan keeps the largest groups it found.
        if !members.is_empty() {
            writes.classes.push((size, members));
        }
        writes.flush_when_due(&mut result)?;
    }

    // ── Phase 4: prune what no longer holds ─────────────────────────────────
    // Only reached when the phases above completed: a cancelled scan leaves
    // stale groups for the next complete run rather than a half-pruned state.
    writes.flush(&mut result, Some((scope.as_ref(), reporter)))?;

    Ok(result)
}

/// What the scan found and has not written yet, and what its writes so far
/// decided. Written by [`Writes::flush`], the only place the scan takes the
/// store after its first reads.
struct Writes<'a> {
    repo: &'a RepoState,
    context: crate::log::WriteContext,
    /// Computed hashes: `(record, field, hash, the stamp it was taken under)`.
    hashes: Vec<(Uuid, &'static str, String, (i64, i64))>,
    /// Completed size classes, their members sharing a full hash with another.
    classes: Vec<(i64, Vec<Candidate>)>,
    /// The records placed in a group, and the groups written, by every write
    /// so far: what the prune keeps.
    grouped: HashSet<Uuid>,
    touched: HashSet<Uuid>,
    opened: Instant,
}

impl Writes<'_> {
    fn flush_when_due(&mut self, result: &mut ScanResult) -> Result<(), ApiError> {
        if self.opened.elapsed() >= COMMIT_EVERY {
            self.flush(result, None)?;
        }
        Ok(())
    }

    /// Takes the store and writes what is pending, then the prune when `prune`
    /// is given (the scan's last write). The store was free since the last
    /// write, so nothing loaded before it is trusted here: the groups and the
    /// links are read again, a record gone since gets no hash, and a group
    /// member is checked on disk — a file changed after it was hashed is left
    /// out, and its change, flushed by the watcher after this write, finds it
    /// in no group (doc "The duplicate scan").
    fn flush(
        &mut self,
        result: &mut ScanResult,
        prune: Option<(Option<&HashSet<Uuid>>, &Reporter)>,
    ) -> Result<(), ApiError> {
        let repo = self.repo;
        let mut conn = metafolder_core::slowlog::timed("wait:conn", || repo.conn.lock_recover());
        let mut existing = crate::store::Questions::duplicate_groups(&*conn)?;
        let mut links = crate::store::Questions::ref_map(&*conn, GROUP_FIELD)?;
        let mut writer = Writer::begin_in(&mut *conn, self.context.clone(), repo.log_retention())?;
        for (uuid, field, hash, stamp) in self.hashes.drain(..) {
            if crate::store::Rows::version(writer.store(), uuid)?.is_some() {
                write_hash(&mut writer, uuid, field, &hash, stamp)?;
            }
        }
        let root = &repo.config.root;
        for (size, mut members) in self.classes.drain(..) {
            let mut kept = Vec::with_capacity(members.len());
            for member in members.drain(..) {
                let abs = root.join(member.rel.trim_start_matches('/'));
                let live = std::fs::symlink_metadata(&abs).ok().map(|meta| {
                    (meta.modified().map(ms_from_systemtime).unwrap_or_default(), meta.len() as i64)
                });
                let exists = crate::store::Rows::version(writer.store(), member.uuid)?.is_some();
                if exists && live.is_some() && live == member.stamp {
                    kept.push(member);
                }
            }
            retain_shared(&mut kept, |m| m.full.clone());
            write_class_groups(
                &mut writer,
                size,
                &kept,
                &mut existing,
                &mut links,
                &mut self.grouped,
                &mut self.touched,
                result,
            )?;
        }
        if let Some((scope, reporter)) = prune {
            prune_groups(
                &mut writer,
                &existing,
                &mut links,
                &self.grouped,
                &self.touched,
                scope,
                reporter,
            )?;
        }
        if writer.op_count() > 0 {
            self.context = crate::log::WriteContext::In(writer.label_id());
        }
        commit(writer)?;
        self.opened = Instant::now();
        Ok(())
    }
}

/// Which fingerprint step a [`hash_step`] call performs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fingerprint {
    Partial,
    Full,
}

/// The outcome of one fingerprint step.
enum Step {
    /// The stored hash was still valid for the file on disk, whose
    /// `(mtime_ms, size)` this is.
    Reused(String, (i64, i64)),
    /// Computed afresh, with the `(mtime_ms, size)` it was computed under.
    Computed(String, (i64, i64)),
}

/// Computes one fingerprint, or reuses the stored one when its stamp still
/// matches the live `stat`.
///
/// The stamp (`mfr_hash_mtime` + `mfr_hash_size`) is what makes reuse safe in a
/// subtree whose watching is off — there, no `Modify` event ever invalidates
/// the hashes. Using mtime this way does not contradict doc "File fingerprint"'s
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
                return Ok(Step::Reused(hash.clone(), live));
            }
        }
    }
    let hash = match which {
        Fingerprint::Partial => fingerprint::partial_hash(&abs)?,
        Fingerprint::Full => fingerprint::full_hash(&abs)?,
    };
    Ok(Step::Computed(hash, live))
}

/// Writes a computed hash and the stamp it was computed under, together, so a
/// stamp can never describe a hash that is not there.
fn write_hash(
    writer: &mut Writer,
    uuid: Uuid,
    name: &str,
    hash: &str,
    (mtime, size): (i64, i64),
) -> Result<(), ApiError> {
    for (field, value) in [
        (name, Value::String(hash.to_string())),
        ("mfr_hash_mtime", Value::DateTime(mtime)),
        ("mfr_hash_size", Value::Int(size)),
    ] {
        writer.set_field_as(OpType::FileModified, uuid, field, value).map_err(ApiError::from)?;
    }
    Ok(())
}

/// Commits one window of the scan's writes as a write; an empty one writes
/// nothing (`Writer::commit` drops it).
fn commit(writer: Writer) -> Result<(), ApiError> {
    metafolder_core::slowlog::timed("commit", || writer.commit()).map_err(ApiError::from)
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
/// several names, and removing a name frees nothing (doc "What counts as a duplicate"). A member
/// with no `mfr_inode` has a single name, so it counts as its
/// own inode.
fn reclaimable_of(size: i64, members: &[Candidate]) -> i64 {
    reclaimable_from_inodes(size, members.iter().map(|m| m.inode.as_deref()))
}

/// [`reclaimable_of`] over bare inode identities, so the incremental update
/// (which reads them back from the database) counts exactly as the scan does.
fn reclaimable_from_inodes<'a>(size: i64, inodes: impl Iterator<Item = Option<&'a str>>) -> i64 {
    let mut distinct: HashSet<&str> = HashSet::new();
    let mut singles = 0i64;
    for inode in inodes {
        match inode {
            Some(inode) => {
                distinct.insert(inode);
            }
            None => singles += 1,
        }
    }
    size * (distinct.len() as i64 + singles - 1).max(0)
}

/// Takes `uuid` out of the duplicate group it belongs to, if any, and leaves the
/// group's stored counters true (doc "Duplicate groups").
///
/// Every way of ceasing to be a live duplicate goes through here — the watcher's
/// removals and content changes, `mf orphan clear`, a revert's path clearing —
/// so a group never outlives its members' departure by a whole scan. Writing
/// only the link and letting the numbers rot was the old rule; a count the
/// interfaces display is not an internal detail, and "stale but harmless" is
/// false the moment it is shown to someone deciding what to delete.
pub fn leave_group(writer: &mut Writer, op: OpType, uuid: Uuid) -> anyhow::Result<()> {
    let group = crate::store::Rows::rows_named(writer.store(), uuid, GROUP_FIELD)?
        .into_iter()
        .find_map(|row| match row.value {
            Value::Ref(group) => Some(group),
            _ => None,
        });
    writer.clear_field_as(op, uuid, GROUP_FIELD)?;
    match group {
        Some(group) => refresh_group(writer, op, group),
        None => Ok(()),
    }
}

/// Recomputes one group's counters from the members that still point at it, and
/// dissolves it when fewer than two remain: a file with no twin is not a
/// duplicate, so the survivor's own link goes with the group metarecord
/// (doc "Duplicate groups").
pub fn refresh_group(writer: &mut Writer, op: OpType, group: Uuid) -> anyhow::Result<()> {
    let members = crate::store::Questions::duplicate_group_members(writer.store(), group)?;
    if members.len() < 2 {
        for member in members {
            writer.clear_field_as(op, member, GROUP_FIELD)?;
        }
        // The group may already be gone (two members of one group departing in
        // the same flush); deleting it twice is an error, not a no-op.
        if crate::store::Rows::version(writer.store(), group)?.is_some() {
            writer.delete_metarecord(group)?;
        }
        return Ok(());
    }
    let size = int_field(writer.store(), group, "mfr_content_size")?.unwrap_or(0);
    let mut inodes = Vec::with_capacity(members.len());
    for &member in &members {
        inodes.push(
            crate::store::Rows::rows_named(writer.store(), member, "mfr_inode")?
                .into_iter()
                .find_map(|row| match row.value {
                    Value::String(inode) => Some(inode),
                    _ => None,
                }),
        );
    }
    let count = members.len() as i64;
    let reclaimable = reclaimable_from_inodes(size, inodes.iter().map(|i| i.as_deref()));
    // Only what changed: removing one of two hard-linked names drops the count
    // without freeing a byte, and a redundant write would be a redundant
    // operation in the log.
    let stored_count = int_field(writer.store(), group, "mfr_duplicate_count")?;
    let stored_reclaimable = int_field(writer.store(), group, "mfr_duplicate_reclaimable")?;
    if stored_count != Some(count) {
        writer.set_field_as(op, group, "mfr_duplicate_count", Value::Int(count))?;
    }
    if stored_reclaimable != Some(reclaimable) {
        writer.set_field_as(op, group, "mfr_duplicate_reclaimable", Value::Int(reclaimable))?;
    }
    Ok(())
}

/// The first `Int` value of `name` on `uuid`, if it has one.
fn int_field(
    conn: &dyn crate::store::Store,
    uuid: Uuid,
    name: &str,
) -> anyhow::Result<Option<i64>> {
    Ok(crate::store::Rows::rows_named(conn, uuid, name)?.into_iter().find_map(|row| {
        match row.value {
            Value::Int(v) => Some(v),
            _ => None,
        }
    }))
}

/// Writes one size class's groups into the scan's open revision:
/// find-or-create each group by its `(size, hash)` identity, refresh its
/// counters, and point every member at it.
#[allow(clippy::too_many_arguments)]
fn write_class_groups(
    writer: &mut Writer,
    size: i64,
    members: &[Candidate],
    existing: &mut HashMap<(i64, String), rows::DuplicateGroup>,
    links: &mut HashMap<Uuid, Uuid>,
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
                stamp: None,
            })
            .collect();
        let reclaimable = reclaimable_of(size, &owned);
        let count = group_members.len() as i64;

        // Identity is the pair (size, hash), not the hash alone: two different
        // size classes could in principle produce the same hash, and the
        // find-or-create has to stay well defined.
        let key = (size, hash.clone());
        let entry = match existing.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let created = writer
                    .create_metarecord(vec![
                        Field::new("mf_schema", Value::String(GROUP_SCHEMA.to_string())),
                        Field::new("mfr_content_hash", Value::String(hash.clone())),
                        Field::new("mfr_content_size", Value::Int(size)),
                    ])
                    .map_err(ApiError::from)?
                    .uuid;
                e.insert(rows::DuplicateGroup { uuid: created, count: None, reclaimable: None })
            }
        };
        let group = entry.uuid;
        // Writing only what changed is what makes a re-scan of an unchanged
        // repository produce no operation at all — `Writer::commit` then drops
        // the empty write. The comparison is against the values loaded up
        // front, so it costs no read here.
        if entry.count != Some(count) {
            writer
                .set_field_as(OpType::FileModified, group, "mfr_duplicate_count", Value::Int(count))
                .map_err(ApiError::from)?;
            entry.count = Some(count);
        }
        if entry.reclaimable != Some(reclaimable) {
            writer
                .set_field_as(
                    OpType::FileModified,
                    group,
                    "mfr_duplicate_reclaimable",
                    Value::Int(reclaimable),
                )
                .map_err(ApiError::from)?;
            entry.reclaimable = Some(reclaimable);
        }
        touched.insert(group);

        for member in group_members {
            if links.get(&member.uuid) != Some(&group) {
                writer
                    .set_field_as(
                        OpType::FileModified,
                        member.uuid,
                        "mfr_duplicate_group",
                        Value::Ref(group),
                    )
                    .map_err(ApiError::from)?;
                links.insert(member.uuid, group);
            }
            grouped.insert(member.uuid);
        }
        result.groups += 1;
        result.files += count as usize;
        result.reclaimable += reclaimable;
    }
    Ok(())
}

/// Removes what the scan disproved: the group link of a record the scan did not
/// place in a group, and every group left with fewer than two members.
#[allow(clippy::too_many_arguments)]
fn prune_groups(
    writer: &mut Writer,
    existing: &HashMap<(i64, String), rows::DuplicateGroup>,
    links: &mut HashMap<Uuid, Uuid>,
    grouped: &HashSet<Uuid>,
    touched: &HashSet<Uuid>,
    scope: Option<&HashSet<Uuid>>,
    reporter: &Reporter,
) -> Result<(), ApiError> {
    let total = existing.len() as u64;
    reporter.progress("prune", Some(0), Some(total));

    // Driven by the records that *currently carry a link* — the map loaded at
    // the start and kept current by the writes above, so this asks the database
    // nothing. It catches every way of ceasing to be a duplicate at once: the
    // twin changed, the file grew out of its size class, it fell under
    // `min_size`, it stopped being a file. That is what keeps
    // "`mfr_duplicate_group` present ⟹ it had a twin at the last scan" true.
    // A scoped scan touches only its own subtree.
    let stale: Vec<Uuid> = links
        .keys()
        .copied()
        .filter(|uuid| !grouped.contains(uuid) && scope.is_none_or(|s| s.contains(uuid)))
        .collect();
    for uuid in stale {
        writer
            .clear_field_as(OpType::FileModified, uuid, "mfr_duplicate_group")
            .map_err(ApiError::from)?;
        links.remove(&uuid);
    }

    for (i, group) in existing.values().enumerate() {
        if i % PROGRESS_STEP == 0 {
            reporter.progress("prune", Some(i as u64), Some(total));
        }
        if touched.contains(&group.uuid) {
            continue;
        }
        let members: Vec<Uuid> = links
            .iter()
            .filter(|(_, target)| **target == group.uuid)
            .map(|(owner, _)| *owner)
            .collect();
        if members.len() > 1 {
            continue;
        }
        for member in members {
            writer
                .clear_field_as(OpType::FileModified, member, "mfr_duplicate_group")
                .map_err(ApiError::from)?;
            links.remove(&member);
        }
        writer.delete_metarecord(group.uuid).map_err(ApiError::from)?;
    }
    Ok(())
}

fn cancelled() -> ApiError {
    ApiError::conflict("duplicate scan cancelled")
}
