//! Pending-event executor (spec-file-tracking "Event batching"): the watcher
//! enqueues raw filesystem events into the persistent `pending_operation`
//! table; after a quiet period the executor compacts them, groups them by
//! resulting operation type (one revision per group), and applies the event
//! semantics to the data tables through the logged write flow.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use rusqlite::{params, Connection};
use uuid::Uuid;

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_core::sync::MutexExt;

use crate::db;
use crate::eligibility;
use crate::fingerprint;
use crate::fs_meta;
use crate::log::{OpType, Writer};
use crate::relpath::RelPath;
use crate::state::RepoState;
use crate::tree_cache::TreeCache;

/// A raw filesystem event, as enqueued by the watcher. Paths are
/// repo-root-relative with a leading `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    Create(RelPath),
    Remove(RelPath),
    /// Correlated rename: both paths are inside the repository.
    Rename(RelPath, RelPath),
    /// The file definitively left the repository.
    RenameFrom(RelPath),
    /// The file arrived from outside the repository.
    RenameTo(RelPath),
    ModifyData(RelPath),
    ModifyMeta(RelPath),
}

/// Appends one event to the persistent buffer. `tracker` is the rename
/// correlation cookie (notify's inotify cookie) for `RenameFrom`/`RenameTo`
/// events, used by [`correlate_renames`] to fuse a split rename; `None` for
/// everything else.
pub fn enqueue(conn: &Connection, ev: &FsEvent, tracker: Option<i64>) -> Result<()> {
    let (op_type, path, from, to): (&str, Option<&RelPath>, Option<&RelPath>, Option<&RelPath>) =
        match ev {
            FsEvent::Create(p) => ("fs_create", Some(p), None, None),
            FsEvent::Remove(p) => ("fs_remove", Some(p), None, None),
            FsEvent::Rename(a, b) => ("fs_rename", None, Some(a), Some(b)),
            FsEvent::RenameFrom(p) => ("fs_rename_from", Some(p), None, None),
            FsEvent::RenameTo(p) => ("fs_rename_to", Some(p), None, None),
            FsEvent::ModifyData(p) => ("fs_modify_data", Some(p), None, None),
            FsEvent::ModifyMeta(p) => ("fs_modify_meta", Some(p), None, None),
        };
    // Both forms: the text keeps the buffer readable, the bytes are what
    // re-opens the file when its name does not decode.
    conn.execute(
        "INSERT INTO pending_operation
             (op_type, path, from_path, to_path, tracker,
              path_bytes, from_path_bytes, to_path_bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            op_type,
            path.map(RelPath::display),
            from.map(RelPath::display),
            to.map(RelPath::display),
            tracker,
            path.map(RelPath::to_bytes),
            from.map(RelPath::to_bytes),
            to.map(RelPath::to_bytes),
        ],
    )?;
    Ok(())
}

/// Appends a whole batch of events in **one transaction**.
///
/// The watcher's own path: `notify` delivers events in a stream, and buffering
/// them one at a time means one transaction — hence, in WAL mode, one fsync —
/// per event. Measured on the machine this was written on: 3 ms per event that
/// way against 0.011 ms batched, so a mass arrival spends minutes buffering
/// (holding the repository's connection) before the flush that applies it even
/// begins. The batch is atomic: a failure leaves none of it buffered, and the
/// events are then simply lost like any event the watcher misses — a reconcile
/// recovers them.
pub fn enqueue_all(conn: &mut Connection, events: &[(FsEvent, Option<i64>)]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for (ev, tracker) in events {
        enqueue(&tx, ev, *tracker)?;
    }
    tx.commit()?;
    Ok(())
}

/// The buffered events (each with its optional tracker id) and the highest
/// `pending_operation` row id read — the flush deletes up to that id, so events
/// enqueued while it runs survive for the next round.
type PendingBatch = (Vec<(FsEvent, Option<i64>)>, i64);

fn load_pending(conn: &Connection) -> Result<PendingBatch> {
    let mut stmt = conn.prepare(
        "SELECT id, op_type, path, from_path, to_path, tracker,
                path_bytes, from_path_bytes, to_path_bytes
         FROM pending_operation WHERE op_type LIKE 'fs_%' ORDER BY id",
    )?;
    let mut events = Vec::new();
    let mut max_id = 0;
    // The bytes are authoritative; the text is the fallback for a row buffered
    // by a daemon that predates them (its path was necessarily valid UTF-8).
    let path_of = |text: Option<String>, bytes: Option<Vec<u8>>| match bytes {
        Some(bytes) => Some(RelPath::from_bytes(&bytes)),
        None => text.map(|t| RelPath::from_display(&t)),
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            path_of(r.get(2)?, r.get(6)?),
            path_of(r.get(3)?, r.get(7)?),
            path_of(r.get(4)?, r.get(8)?),
            r.get::<_, Option<i64>>(5)?,
        ))
    })?;
    for row in rows {
        let (id, op, path, from, to, tracker) = row?;
        max_id = max_id.max(id);
        let p = || path.clone().context("missing path in pending_operation");
        let ev = match op.as_str() {
            "fs_create" => FsEvent::Create(p()?),
            "fs_remove" => FsEvent::Remove(p()?),
            "fs_rename" => FsEvent::Rename(
                from.clone().context("missing from_path")?,
                to.clone().context("missing to_path")?,
            ),
            "fs_rename_from" => FsEvent::RenameFrom(p()?),
            "fs_rename_to" => FsEvent::RenameTo(p()?),
            "fs_modify_data" => FsEvent::ModifyData(p()?),
            "fs_modify_meta" => FsEvent::ModifyMeta(p()?),
            other => anyhow::bail!("unknown pending op_type '{other}'"),
        };
        events.push((ev, tracker));
    }
    Ok((events, max_id))
}

/// Fuses a rename that the notify backend delivered as separate
/// `RenameFrom`/`RenameTo` events (its From/To correlation can fail under load)
/// back into a single [`FsEvent::Rename`], by pairing events that carry the
/// same non-null inotify cookie. Without this, an intra-tree rename would
/// degrade into a delete + arrival (two revisions; identity preserved only if
/// the content is unchanged, via the fingerprint search). Events without a
/// cookie, and genuine one-sided renames (a file that left or entered the
/// repository, with no matching cookie), are left untouched.
fn correlate_renames(events: Vec<(FsEvent, Option<i64>)>) -> Vec<FsEvent> {
    let mut slots: Vec<Option<(FsEvent, Option<i64>)>> = events.into_iter().map(Some).collect();
    // Departures still looking for their other half, per cookie, most recent
    // last. Scanning backwards for one instead is quadratic in the batch, and
    // the worst case is the ordinary one: files moved in from *outside* the
    // repository carry a cookie whose `RenameFrom` will never be in the batch,
    // so every one of them walks the whole batch to find nothing.
    let mut waiting: HashMap<i64, Vec<usize>> = HashMap::new();
    for i in 0..slots.len() {
        match &slots[i] {
            Some((FsEvent::RenameFrom(_), Some(cookie))) => {
                waiting.entry(*cookie).or_default().push(i);
                continue;
            }
            Some((FsEvent::RenameTo(_), Some(_))) => {}
            _ => continue,
        }
        let (to_path, cookie) = match &slots[i] {
            Some((FsEvent::RenameTo(b), Some(cookie))) => (b.clone(), *cookie),
            _ => unreachable!("filtered to a cookied RenameTo above"),
        };
        // The most recent earlier RenameFrom sharing this cookie is the source
        // (inotify cookies are unique per rename); taking it removes it from
        // the waiting set, so it pairs at most once.
        let Some(j) = waiting.get_mut(&cookie).and_then(Vec::pop) else { continue };
        let from_path = match slots[j].take() {
            Some((FsEvent::RenameFrom(a), _)) => a,
            _ => unreachable!("only unpaired RenameFrom events are registered"),
        };
        slots[i] = Some((FsEvent::Rename(from_path, to_path), None));
    }
    slots.into_iter().flatten().map(|(ev, _)| ev).collect()
}

/// Compaction rules of spec-file-tracking: redundant sequences within the
/// batching window are simplified before any database write.
///
/// Each rule looks for the *last* live earlier event matching a path in a
/// particular role (a `Create`'s path, a `Rename`'s to-path, a `RenameFrom`/
/// `RenameTo`'s path, a `Modify`'s path). Instead of an O(n²) `rposition` scan
/// per event, we keep one `path -> {live indices}` set per role: the set's max
/// is exactly that "last matching", and every null-out / in-place rewrite / push
/// updates the sets so lookups stay O(log n). Validated to match the former
/// linear-scan implementation byte-for-byte by the `compact_matches_reference`
/// fuzz test.
fn compact(events: Vec<FsEvent>) -> Vec<FsEvent> {
    use std::collections::{BTreeSet, HashMap};

    /// Highest live index registered for `key`, or `None` (the `find_last`).
    fn last(map: &HashMap<RelPath, BTreeSet<usize>>, key: &RelPath) -> Option<usize> {
        map.get(key).and_then(|s| s.last().copied())
    }
    fn insert(map: &mut HashMap<RelPath, BTreeSet<usize>>, key: &RelPath, i: usize) {
        map.entry(key.clone()).or_default().insert(i);
    }
    fn unregister(map: &mut HashMap<RelPath, BTreeSet<usize>>, key: &RelPath, i: usize) {
        if let Some(s) = map.get_mut(key) {
            s.remove(&i);
        }
    }
    fn any(map: &HashMap<RelPath, BTreeSet<usize>>, key: &RelPath) -> bool {
        map.get(key).is_some_and(|s| !s.is_empty())
    }

    let mut out: Vec<Option<FsEvent>> = Vec::with_capacity(events.len());
    // One live-index set per lookup role (keyed by the path that role uses).
    let mut create: HashMap<RelPath, BTreeSet<usize>> = HashMap::new();
    let mut rename_from: HashMap<RelPath, BTreeSet<usize>> = HashMap::new();
    let mut rename_to: HashMap<RelPath, BTreeSet<usize>> = HashMap::new();
    let mut rename_by_to: HashMap<RelPath, BTreeSet<usize>> = HashMap::new();
    // Renames indexed by their *source*, to spot the collapse that would create
    // a cycle (see the chain branch below).
    let mut rename_by_from: HashMap<RelPath, BTreeSet<usize>> = HashMap::new();
    let mut modify_data: HashMap<RelPath, BTreeSet<usize>> = HashMap::new();
    let mut modify_meta: HashMap<RelPath, BTreeSet<usize>> = HashMap::new();

    for ev in events {
        match ev {
            FsEvent::Remove(p) => {
                // Create A, Remove A → nothing. (Remove is never looked up, so
                // when it survives it needs no index.)
                if let Some(i) = last(&create, &p) {
                    out[i] = None;
                    unregister(&mut create, &p, i);
                } else {
                    out.push(Some(FsEvent::Remove(p)));
                }
            }
            FsEvent::Rename(a, b) => {
                // The notify backend emits Rename(From) + Rename(To) and then the
                // correlated Rename(Both) for the same move: the one-sided pair is
                // absorbed by the Both event.
                if let Some(i) = last(&rename_from, &a) {
                    out[i] = None;
                    unregister(&mut rename_from, &a, i);
                }
                if let Some(i) = last(&rename_to, &b) {
                    out[i] = None;
                    unregister(&mut rename_to, &b, i);
                }
                if let Some(i) = last(&create, &a) {
                    // Create A, Rename A→B → Create B.
                    out[i] = Some(FsEvent::Create(b.clone()));
                    unregister(&mut create, &a, i);
                    insert(&mut create, &b, i);
                } else if let Some(i) = last(&rename_by_to, &a)
                    .filter(|&i| last(&rename_by_from, &b).is_none_or(|j| j <= i))
                {
                    // Rename X→A, Rename A→B → Rename X→B.
                    //
                    // Not when B is the source of a rename that has not been
                    // applied yet (a later index): collapsing would hoist the
                    // arrival at B in front of B's own departure, and the two
                    // renames would form a cycle — the swap `a→tmp, b→a, tmp→b`
                    // becomes `a→b, b→a`, which no sequential order can honour
                    // (one metarecord per tree position). Keeping the hop
                    // through the intermediate path costs one extra `file_moved`
                    // and breaks the cycle, exactly as it did on disk.
                    let Some(FsEvent::Rename(x, _)) = out[i].clone() else { unreachable!() };
                    out[i] = Some(FsEvent::Rename(x, b.clone()));
                    unregister(&mut rename_by_to, &a, i);
                    insert(&mut rename_by_to, &b, i);
                } else {
                    insert(&mut rename_by_to, &b, out.len());
                    insert(&mut rename_by_from, &a, out.len());
                    out.push(Some(FsEvent::Rename(a, b)));
                }
            }
            FsEvent::ModifyData(p) => {
                // Create A, Modify A → Create A; Modify ×N → one Modify.
                if !(any(&create, &p) || any(&modify_data, &p)) {
                    insert(&mut modify_data, &p, out.len());
                    out.push(Some(FsEvent::ModifyData(p)));
                }
            }
            FsEvent::ModifyMeta(p) => {
                if !(any(&create, &p) || any(&modify_meta, &p)) {
                    insert(&mut modify_meta, &p, out.len());
                    out.push(Some(FsEvent::ModifyMeta(p)));
                }
            }
            FsEvent::Create(p) => {
                insert(&mut create, &p, out.len());
                out.push(Some(FsEvent::Create(p)));
            }
            FsEvent::RenameFrom(p) => {
                insert(&mut rename_from, &p, out.len());
                out.push(Some(FsEvent::RenameFrom(p)));
            }
            FsEvent::RenameTo(p) => {
                insert(&mut rename_to, &p, out.len());
                out.push(Some(FsEvent::RenameTo(p)));
            }
        }
    }
    out.into_iter().flatten().collect()
}

/// Groups for revision splitting; each group becomes one revision. Note:
/// arrivals (`Rename(To)`) form their own group because their resulting op
/// type (create vs file_moved) is only known per file at apply time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    Create,
    Delete,
    Move,
    Modify,
    Arrival,
}

fn group_kind(ev: &FsEvent) -> GroupKind {
    match ev {
        FsEvent::Create(_) => GroupKind::Create,
        FsEvent::Remove(_) | FsEvent::RenameFrom(_) => GroupKind::Delete,
        FsEvent::Rename(_, _) => GroupKind::Move,
        FsEvent::ModifyData(_) | FsEvent::ModifyMeta(_) => GroupKind::Modify,
        FsEvent::RenameTo(_) => GroupKind::Arrival,
    }
}

#[derive(Debug, Default)]
pub struct FlushStats {
    pub events: usize,
    pub revisions: usize,
    /// The flush was stopped at the user's request: the group it was applying
    /// was abandoned, the buffer was left in place, and ingestion is now paused
    /// (spec-file-tracking "Pausing ingestion").
    pub cancelled: bool,
}

/// The error a flush unwinds with when it observes a cancellation request.
/// Carried through `anyhow` and recognised by downcast, so every `?` on the
/// way out abandons the group's transaction exactly as a failure would — but
/// the outcome is a pause, not a failure.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("flush cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Whether an error is the cancellation marker.
fn is_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Cancelled>().is_some()
}

/// How many times a batch the executor cannot apply is retried before it is
/// dropped. The pending buffer is persistent so that a crash loses no event —
/// which also means a batch that always fails is retried for ever, and while it
/// is stuck *nothing* is recorded for the repository any more, not even after a
/// restart (the buffer is replayed at load). Dropping the batch loses those
/// events; a reconcile recovers them, where a dead watcher recovers nothing.
pub const FLUSH_FAILURE_BUDGET: u32 = 3;

/// Processes the whole pending buffer: compaction, grouping, application.
/// Also used at load time to replay a buffer left by a previous daemon run.
///
/// On failure the batch is left in place to be retried, until the failure
/// budget runs out (see [`FLUSH_FAILURE_BUDGET`]) — then it is dropped, loudly,
/// so a single unapplicable batch cannot silently switch tracking off for good.
pub fn flush_pending(repo: &RepoState) -> Result<FlushStats> {
    use std::sync::atomic::Ordering;
    match flush_pending_once(repo) {
        Ok(stats) => {
            if !stats.cancelled {
                repo.flush_failures.store(0, Ordering::Relaxed);
            }
            Ok(stats)
        }
        Err(err) => {
            let failures = repo.flush_failures.fetch_add(1, Ordering::Relaxed) + 1;
            if failures >= FLUSH_FAILURE_BUDGET {
                repo.flush_failures.store(0, Ordering::Relaxed);
                let dropped = discard_pending(repo).unwrap_or(0);
                crate::diagnostics::error(
                    "executor",
                    format!(
                        "dropping {dropped} filesystem event(s) after {failures} failed \
                         flushes ({err:#}); run a reconcile to pick up what they carried"
                    ),
                );
            }
            Err(err)
        }
    }
}

/// Deletes the buffered filesystem events, returning how many were dropped.
/// The restoration ops (rollback skips) are left alone: they are generated by
/// the daemon itself, not by the filesystem.
fn discard_pending(repo: &RepoState) -> Result<usize> {
    let conn = repo.conn.lock_recover();
    Ok(conn.execute("DELETE FROM pending_operation WHERE op_type LIKE 'fs_%'", [])?)
}

fn flush_pending_once(repo: &RepoState) -> Result<FlushStats> {
    // While a coordinated rollback holds the lock, pending operations (watcher
    // events and restoration ops) accumulate but are not committed; they are
    // replayed once the lock is released (spec-event-log "Rollback lock").
    if repo.is_rollback_locked() {
        return Ok(FlushStats::default());
    }
    // Ingestion paused (spec-file-tracking "Pausing ingestion"): the watcher
    // keeps buffering, nothing is applied until a resume.
    if repo.is_ingestion_paused() {
        return Ok(FlushStats::default());
    }
    let mut conn = repo.conn.lock_recover();
    let mut cache = repo.lock_cache();

    // Restoration ops from skipped rollback steps are replayed first, as their
    // own revision, before the watcher events recorded during the lock.
    let mut revisions_from_restore = 0;
    revisions_from_restore += flush_restorations(&mut conn, &mut cache)?;

    let (events, max_id) = load_pending(&conn)?;
    if events.is_empty() {
        return Ok(FlushStats { events: 0, revisions: revisions_from_restore, cancelled: false });
    }
    let events = compact(correlate_renames(events));
    let n_events = events.len();

    // Paths renamed away with no matching arrival in this batch. Either the
    // file really left the repository, or it moved into a directory the watcher
    // was not yet watching — `Apply::find_departed_match` tells the two apart
    // when the destination turns up in a new directory's scan.
    let departed: Vec<RelPath> = events
        .iter()
        .filter_map(|ev| match ev {
            FsEvent::RenameFrom(p) => Some(p.clone()),
            _ => None,
        })
        .collect();

    // Group by kind, keeping groups ordered by first occurrence.
    let mut groups: Vec<(GroupKind, Vec<FsEvent>)> = Vec::new();
    for ev in events {
        let kind = group_kind(&ev);
        match groups.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, list)) => list.push(ev),
            None => groups.push((kind, vec![ev])),
        }
    }

    // The declared-but-unmounted mount points, resolved once for the whole
    // flush: one stat pair per mount point, and none at all in the usual case
    // where the repository declares none.
    let offline = crate::mount::offline(&conn, &mut cache, &repo.config.root)?;

    // Observable while the events are applied (spec-tasks). Registered only
    // now that there is real work (non-empty events), to avoid churning the
    // registry with no-op flushes.
    let task = repo.tasks.start(crate::tasks::TaskKind::Flush);
    repo.tasks.mark_running(task);
    repo.tasks.set_progress(task, "flush", None, None);

    // Ancestor watch/ignore reads and compiled patterns, shared by every event
    // of the flush: a per-event cache re-reads the chain and recompiles each
    // `mf_ignore` pattern for every single file.
    let mut elig = eligibility::EligibilityCache::default();

    // Cooperative stop: the registry flag is read at every event boundary (and
    // inside the two loops one event can hide a whole tree behind — a directory
    // scan and an orphan cascade).
    // Either the task's own flag, or a pause that arrived while this flush was
    // still loading and compacting its batch — before the task existed for the
    // route to cancel. Without the second, a stop landing in that window would
    // be silently ignored, which on a huge batch is a window of seconds.
    let cancel = || repo.tasks.is_cancel_requested(task) || repo.is_ingestion_paused();

    let work = (|| -> Result<usize> {
        let mut revisions = 0;
        for (_, group) in groups {
            let writer = Writer::begin(&mut conn, None)?;
            let mut apply = Apply {
                writer,
                cache: &mut cache,
                root: &repo.config.root,
                departed: &departed,
                offline: &offline,
                orphan_limit: repo.orphan_cascade_limit,
                departed_index: None,
                elig: &mut elig,
                orphan_index: None,
                cancel: &cancel,
            };
            for ev in group {
                apply.check_cancelled()?;
                apply.apply(ev)?;
            }
            let wrote = apply.writer.op_count() > 0;
            apply.writer.commit()?;
            if wrote {
                revisions += 1;
            }
        }

        conn.execute(
            "DELETE FROM pending_operation WHERE id <= ?1 AND op_type LIKE 'fs_%'",
            params![max_id],
        )?;
        Ok(revisions)
    })();

    match work {
        Ok(revisions) => {
            repo.tasks.finish(task, None);
            Ok(FlushStats {
                events: n_events,
                revisions: revisions + revisions_from_restore,
                cancelled: false,
            })
        }
        // Stopped by the user: the group in progress rolled back with its
        // writer, the pending buffer is untouched (it is deleted only on a
        // completed flush), and ingestion stays paused until a resume — without
        // that, the next event would restart the flush just stopped.
        Err(e) if is_cancelled(&e) => {
            resync_cache(&conn, &mut cache);
            repo.pause_ingestion();
            repo.tasks.mark_cancelled(task);
            crate::diagnostics::warn(
                "executor",
                format!(
                    "flush stopped: {n_events} filesystem event(s) left buffered, \
                     ingestion paused until resumed"
                ),
            );
            Ok(FlushStats { events: n_events, revisions: revisions_from_restore, cancelled: true })
        }
        Err(e) => {
            resync_cache(&conn, &mut cache);
            repo.tasks.fail(task, &e.to_string());
            Err(e)
        }
    }
}

/// Rebuilds the tree cache from the database after a group was abandoned.
///
/// The cache is maintained *incrementally* as events are applied (`apply_insert`
/// / `apply_rename` / `apply_remove`), in memory, next to the writes — and a
/// dropped writer rolls the writes back but not the cache. Without this, a
/// stopped (or failed) flush leaves the cache claiming paths no committed
/// metarecord holds, and every later resolution of them answers a uuid that is
/// not there. Rebuilding costs one scan of the forest and only happens on a
/// path that has already given up.
fn resync_cache(conn: &Connection, cache: &mut TreeCache) {
    if let Err(err) = cache.populate(conn) {
        crate::diagnostics::error(
            "executor",
            format!("could not rebuild the tree cache after an abandoned flush: {err:#}"),
        );
    }
}

/// Replays restoration ops left by skipped coordinated-rollback steps as a
/// single revision (spec-event-log "skip"), then deletes them. The tree cache
/// is cleared afterwards because `mfr_path` restorations move tree positions.
fn flush_restorations(conn: &mut Connection, cache: &mut TreeCache) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, op_type, path, from_path, to_path FROM pending_operation
         WHERE op_type LIKE 'restore_%' ORDER BY id",
    )?;
    // (id, op_type, path, from_path, to_path)
    type RestoreRow = (i64, String, Option<String>, Option<String>, Option<String>);
    let rows: Vec<RestoreRow> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    if rows.is_empty() {
        return Ok(0);
    }
    let max_id = rows.iter().map(|r| r.0).max().unwrap_or(0);

    let parse_uuid = |s: &str| -> Result<Uuid> {
        Uuid::parse_str(s).with_context(|| format!("invalid uuid in restoration op: {s}"))
    };

    let mut writer = Writer::begin(conn, None)?;
    for (_, op_type, path, from_path, to_path) in &rows {
        let entity = parse_uuid(path.as_deref().context("restoration op missing entity")?)?;
        match op_type.as_str() {
            "restore_set_path" => {
                let parent = match from_path.as_deref() {
                    Some(p) if !p.is_empty() => Some(parse_uuid(p)?),
                    _ => None,
                };
                let name = to_path.clone().unwrap_or_default();
                writer.set_field_as(
                    OpType::FileMoved,
                    entity,
                    "mfr_path",
                    Value::TreeRef { parent, name: name.into() },
                )?;
            }
            "restore_clear_path" => {
                writer.set_field_as(OpType::FileDeleted, entity, "mfr_path", Value::Nothing)?;
            }
            "restore_clear_hashes" => {
                writer.clear_field_as(OpType::FileModified, entity, "mfr_partial_hash")?;
                writer.clear_field_as(OpType::FileModified, entity, "mfr_full_hash")?;
            }
            other => anyhow::bail!("unknown restoration op_type '{other}'"),
        }
    }
    let wrote = writer.op_count() > 0;
    writer.commit()?;
    // The restore rewrote tree positions arbitrarily: rebuild the cache from
    // the new state (keeps it complete; `populate` clears first).
    cache.populate(conn)?;
    conn.execute(
        "DELETE FROM pending_operation WHERE id <= ?1 AND op_type LIKE 'restore_%'",
        params![max_id],
    )?;
    Ok(if wrote { 1 } else { 0 })
}

/// Application context for one revision (one group of events).
struct Apply<'a, 'c> {
    writer: Writer<'c>,
    cache: &'a mut TreeCache,
    root: &'a Path,
    /// Paths renamed *out of* a watched directory in this same batch, whose
    /// destination the watcher never saw. See [`Apply::find_departed_match`].
    departed: &'a [RelPath],
    /// Mount points that are declared but not mounted right now: every event
    /// landing in one is dropped, because the content behind those paths is
    /// *unavailable*, not gone (spec-file-tracking "Offline subtrees").
    offline: &'a crate::mount::OfflineMounts,
    /// Mass-orphan circuit breaker (0 = disabled).
    orphan_limit: usize,
    /// `departed` indexed by the stat a rename preserves, built on first use.
    /// Without it the pairing costs one stat and one database read per
    /// (arrival, departure) *pair*: a batch that both loses and gains a few
    /// hundred files then holds the connection for minutes, and every query
    /// queues behind it.
    departed_index: Option<HashMap<StatKey, Vec<RelPath>>>,
    /// Ancestor `mf_watch` / `mf_ignore` reads and compiled `mf_ignore`
    /// regexes, shared by every event of the flush (the reconcile walk's
    /// [`eligibility::EligibilityCache`]). Nothing the executor writes changes
    /// those two fields, so no entry of it can go stale mid-flush.
    elig: &'a mut eligibility::EligibilityCache,
    /// The matchable orphans (`mfr_path` = Nothing, both hashes stored) indexed
    /// by size, built on first use and dropped whenever this group orphans a
    /// metarecord (a new orphan belongs in it) or re-homes one (it no longer
    /// does). Without it each arriving file asks the database its own
    /// orphan question, which costs a walk over every tracked file.
    orphan_index: Option<HashMap<i64, Vec<db::OrphanCandidate>>>,
    /// Cooperative stop probe (the flush task's cancellation flag). Read at
    /// every event, and inside the loops that can run long on a single event.
    cancel: &'a dyn Fn() -> bool,
}

/// The stat a rename preserves exactly — kind, size, mtime — as a lookup key.
type StatKey = (String, i64, i64);

/// The key of a record (or of a freshly stat-ed path), or `None` when one of
/// the three is missing or of another type: without all three there is nothing
/// to match a departure against.
fn stat_key(kind: Option<&Value>, size: Option<&Value>, mtime: Option<&Value>) -> Option<StatKey> {
    match (kind, size, mtime) {
        (Some(Value::String(k)), Some(Value::Int(s)), Some(Value::DateTime(m))) => {
            Some((k.clone(), *s, *m))
        }
        _ => None,
    }
}

impl Apply<'_, '_> {
    /// Fails with [`Cancelled`] once the user has asked the flush to stop.
    /// Every caller propagates it with `?`, so the group's writer is dropped
    /// (rolling its transaction back) on the way out.
    fn check_cancelled(&self) -> Result<()> {
        if (self.cancel)() {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    fn apply(&mut self, ev: FsEvent) -> Result<()> {
        // An offline mount point holds no content to observe: whatever the
        // watcher reported there is about a volume that is not plugged in
        // (a stale watch, a replayed buffer, the unmount itself), and acting on
        // it would orphan records whose files are merely unavailable.
        if self.frozen(&ev) {
            return Ok(());
        }
        match ev {
            FsEvent::Create(p) => self.apply_create(&p),
            FsEvent::Remove(p) | FsEvent::RenameFrom(p) => self.apply_remove(&p),
            FsEvent::Rename(a, b) => self.apply_rename(&a, &b),
            FsEvent::RenameTo(p) => self.apply_arrival(&p),
            FsEvent::ModifyData(p) => self.apply_modify_data(&p),
            FsEvent::ModifyMeta(p) => self.apply_modify_meta(&p),
        }
    }

    fn abs(&self, rel: &RelPath) -> std::path::PathBuf {
        rel.to_abs(self.root)
    }

    /// Whether any path of `ev` lies at or below an offline mount point. A
    /// rename is frozen if *either* side is: neither half of it can be trusted
    /// when one end is on a volume that is not there.
    fn frozen(&self, ev: &FsEvent) -> bool {
        if self.offline.is_empty() {
            return false;
        }
        match ev {
            FsEvent::Create(p)
            | FsEvent::Remove(p)
            | FsEvent::RenameFrom(p)
            | FsEvent::RenameTo(p)
            | FsEvent::ModifyData(p)
            | FsEvent::ModifyMeta(p) => self.offline.contains(&p.display()),
            FsEvent::Rename(a, b) => {
                self.offline.contains(&a.display()) || self.offline.contains(&b.display())
            }
        }
    }

    fn eligible(&mut self, rel: &RelPath) -> Result<bool> {
        eligibility::is_eligible_cached(
            self.writer.connection(),
            self.cache,
            &rel.display(),
            self.elig,
        )
    }

    fn resolve(&mut self, rel: &RelPath) -> Result<Option<Uuid>> {
        self.cache.resolve_rel(self.writer.connection(), "mfr_path", rel)
    }

    /// Resolves the parent directory entry of `rel`, creating any missing
    /// intermediate directory metarecords (with their stat fields).
    fn ensure_parents(&mut self, rel: &RelPath) -> Result<Uuid> {
        ensure_parent_metarecords(&mut self.writer, self.cache, self.root, rel, &[])
    }

    fn apply_create(&mut self, rel: &RelPath) -> Result<()> {
        if !self.create_or_refresh(rel)? {
            return Ok(()); // Ineligible: nothing created, nothing to descend into.
        }
        if self.is_dir(rel) {
            self.scan_dir(rel)?;
        }
        Ok(())
    }

    /// Scans a newly-tracked directory subtree and ingests everything already
    /// inside it. A directory pasted or moved in wholesale arrives as a single
    /// Create for the directory, but inotify's recursive watch is only
    /// registered *after* the directory exists — anything already inside it
    /// never fires its own event (the classic recursive-watch race). Children
    /// are ingested with arrival semantics (`ingest_arrival`), so a moved-in
    /// subtree reuses orphaned metarecords by fingerprint rather than
    /// duplicating them. Idempotent with any child events that did fire.
    fn scan_dir(&mut self, rel: &RelPath) -> Result<()> {
        let mut stack = vec![rel.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(self.abs(&dir)) {
                Ok(entries) => entries,
                Err(_) => continue, // Vanished mid-scan; a reconcile can catch up.
            };
            for entry in entries.flatten() {
                // A pasted tree can be arbitrarily large, and it all rides on
                // one event: the stop is honoured here too, not only between
                // events.
                self.check_cancelled()?;
                // A POSIX name is a byte string; such a file is ingested like
                // any other (spec-data-model "Tree names").
                let child = dir.child(TreeName::from_bytes(crate::relpath::file_name_bytes(
                    &entry.file_name(),
                )));
                // Descend into an eligible subdirectory to reach its own
                // contents; an ignored one (and its whole subtree) is skipped.
                if self.ingest_arrival(&child)? && self.is_dir(&child) {
                    stack.push(child);
                }
            }
        }
        Ok(())
    }

    /// Creates or refreshes the metarecord for a single path (no orphan
    /// matching). Returns whether the path was eligible — the caller uses this
    /// to decide whether to descend into a directory.
    fn create_or_refresh(&mut self, rel: &RelPath) -> Result<bool> {
        if !self.eligible(rel)? {
            return Ok(false);
        }
        match self.resolve(rel)? {
            // Already tracked (e.g. replay after crash, or a delivered child
            // event): refresh instead.
            Some(existing) => self.refresh_data(existing, rel)?,
            None => self.create_record(rel)?,
        }
        Ok(true)
    }

    /// Per-path arrival ingest (no subtree scan): refresh a known path, else
    /// reuse an orphaned metarecord when a full-hash fingerprint confirms
    /// identity (files only), otherwise create. Returns whether the path was
    /// eligible.
    fn ingest_arrival(&mut self, rel: &RelPath) -> Result<bool> {
        if !self.eligible(rel)? {
            return Ok(false);
        }
        if let Some(existing) = self.resolve(rel)? {
            self.refresh_data(existing, rel)?;
            return Ok(true);
        }
        let abs = self.abs(rel);
        // lstat: a symlink is `is_file() == false`, so the fingerprint-based
        // orphan search below is skipped and the target is never read.
        let Ok(meta) = std::fs::symlink_metadata(&abs) else {
            return Ok(true); // Vanished before the flush.
        };
        if meta.is_file() {
            if let Some(orphan) = self.find_orphan_match(&abs, meta.len() as i64)? {
                let parent = self.ensure_parents(rel)?;
                let name = rel.name().cloned().unwrap_or_default();
                self.writer.set_field_as(
                    OpType::FileMoved,
                    orphan,
                    "mfr_path",
                    Value::TreeRef { parent: Some(parent), name: name.clone() },
                )?;
                self.cache.apply_insert("mfr_path", Some(parent), &name, orphan);
                // Refresh the stat-derived fields at the new location.
                for field in fs_meta::stat_fields_in(self.root, &abs)? {
                    self.writer.set_field_as(
                        OpType::FileModified,
                        orphan,
                        &field.name,
                        field.value,
                    )?;
                }
                return Ok(true);
            }
        }
        // The other half of a move whose destination the watcher could not see
        // (a directory created in this same batch): re-pair it rather than let
        // the file arrive as a stranger. Tried after the fingerprint search — an
        // exact hash is stronger evidence — and it is the only chance a moved
        // *directory* gets.
        if let Some(from) = self.find_departed_match(rel)? {
            self.apply_rename(&from, rel)?;
            return Ok(true);
        }
        self.create_record(rel)?;
        Ok(true)
    }

    /// Creates a fresh metarecord for `rel`: its `mfr_path` TreeRef plus the
    /// stat-derived fields. A no-op if the path vanished before the flush.
    fn create_record(&mut self, rel: &RelPath) -> Result<()> {
        let Ok(stat) = fs_meta::stat_fields_in(self.root, &self.abs(rel)) else {
            return Ok(()); // The file disappeared before the flush.
        };
        let parent = self.ensure_parents(rel)?;
        let name = rel.name().cloned().unwrap_or_default();
        let mut fields = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: name.clone() },
        )];
        fields.extend(stat);
        let created = self.writer.create_metarecord(fields)?;
        self.cache.apply_insert("mfr_path", Some(parent), &name, created.uuid);
        Ok(())
    }

    /// Whether `rel` is a directory on disk (not following symlinks: a symlink
    /// to a directory is not descended into).
    fn is_dir(&self, rel: &RelPath) -> bool {
        std::fs::symlink_metadata(self.abs(rel)).map(|m| m.is_dir()).unwrap_or(false)
    }

    /// `Remove` / `Rename(From)`: the metarecord is preserved, `mfr_path` becomes
    /// Nothing, and the whole subtree is cleared in the same transaction.
    fn apply_remove(&mut self, rel: &RelPath) -> Result<()> {
        let Some(uuid) = self.resolve(rel)? else {
            return Ok(());
        };
        // The path exists again on disk: the deletion was undone within the
        // batching window — the file was recreated, or a `mf trash -f` +
        // `mf trash restore` round-trip put it back before the buffered
        // `file_deleted` flushed. Orphaning here would strand the live
        // metarecord and let the paired arrival create a duplicate (a
        // freshly-tracked file has no stored full hash for the fingerprint
        // search to re-home it). Skip the stale removal; the arrival/create in
        // the same batch refreshes the still-linked metarecord instead.
        if std::fs::symlink_metadata(self.abs(rel)).is_ok() {
            return Ok(());
        }
        if !self.eligible(rel)? {
            return Ok(()); // Out of watch scope: metadata left unchanged.
        }
        self.orphan_subtree(uuid)
    }

    /// Orphans `uuid` and every descendant: `mfr_path` becomes Nothing, the
    /// metarecords themselves are preserved. Used when the content behind a
    /// path is gone — a deletion, a departure, or a file destroyed by something
    /// else being moved on top of it.
    fn orphan_subtree(&mut self, uuid: Uuid) -> Result<()> {
        let descendants = self.cache.descendants(self.writer.connection(), "mfr_path", uuid)?;
        // Mass-orphan circuit breaker (spec-file-tracking): a single event that
        // would null thousands of paths is a filesystem going away, not a
        // deletion. Skipping is recoverable — the paths stay stale and
        // `mf orphan clear` confirms them after re-checking the disk — whereas
        // the cascade is not.
        let total = descendants.len() + 1;
        if self.orphan_limit > 0 && total > self.orphan_limit {
            crate::diagnostics::warn(
                "executor",
                format!(
                    "refusing to orphan {total} metarecords at once (limit {}): \
                     leaving the paths untouched — confirm with `mf orphan clear` if the \
                     deletion is real",
                    self.orphan_limit
                ),
            );
            return Ok(());
        }
        // Snapshot every path *before* any write: with an incomplete tree cache
        // `path_of` walks the DB, and clearing a parent's `mfr_path` would break
        // its descendants' walk. `mfr_path_old` is a frozen String recording
        // where the orphan last lived (spec-file-tracking "Orphan origin").
        let mut olds = Vec::with_capacity(descendants.len() + 1);
        for &u in std::iter::once(&uuid).chain(descendants.iter()) {
            olds.push((u, self.cache.path_of(self.writer.connection(), "mfr_path", u)?));
        }
        for (u, old) in olds {
            // A cascade over a whole subtree is as long as the subtree.
            self.check_cancelled()?;
            if let Some(old) = old {
                self.writer.set_field_as(
                    OpType::FileDeleted,
                    u,
                    "mfr_path_old",
                    Value::String(old),
                )?;
            }
            self.writer.set_field_as(OpType::FileDeleted, u, "mfr_path", Value::Nothing)?;
        }
        self.cache.apply_remove("mfr_path", uuid);
        // These records are orphans now: a later arrival in this same group may
        // legitimately match one, so the index has to be built again.
        self.orphan_index = None;
        Ok(())
    }

    fn apply_rename(&mut self, from: &RelPath, to: &RelPath) -> Result<()> {
        let Some(src) = self.resolve(from)? else {
            // Unknown source: treat as an arrival at the destination.
            return self.apply_arrival(to);
        };
        if !self.eligible(to)? {
            return Ok(()); // Moved out of scope: keep the stale path.
        }
        // The destination was itself tracked: the rename destroyed the file
        // that was there (`mv a b`, a download, an editor saving atomically), so
        // its metarecord is orphaned exactly as a deletion would — its content
        // is gone. Skipping this is not an option: one metarecord may hold a
        // given tree position, so the move would fail the tree constraint, and
        // with it the whole flush — whose batch is then retried forever, leaving
        // the watcher recording nothing at all for this repository.
        if let Some(occupant) = self.resolve(to)? {
            if occupant != src {
                self.orphan_subtree(occupant)?;
            }
        }
        let parent = self.ensure_parents(to)?;
        let name = to.name().cloned().unwrap_or_default();
        self.writer.set_field_as(
            OpType::FileMoved,
            src,
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: name.clone() },
        )?;
        self.cache.apply_rename("mfr_path", src, Some(parent), &name);
        Ok(())
    }

    /// `Rename(To)`: a path arrived from outside. Reuse an orphaned metarecord
    /// when a full-hash fingerprint confirms identity, otherwise create; a
    /// directory also has its existing contents scanned (the recursive-watch
    /// race — see [`Self::scan_dir`]).
    fn apply_arrival(&mut self, rel: &RelPath) -> Result<()> {
        if !self.ingest_arrival(rel)? {
            return Ok(());
        }
        if self.is_dir(rel) {
            self.scan_dir(rel)?;
        }
        Ok(())
    }

    /// The path a file now arriving at `rel` was renamed away from earlier in
    /// this same batch, if any.
    ///
    /// inotify reports a move as a From/To pair that [`correlate_renames`] fuses
    /// back into one `Rename`. When the destination directory is *itself* new —
    /// "create a folder, drop a file into it", the most ordinary file-manager
    /// gesture — its watch cannot be in place before the kernel has told us the
    /// directory exists, so only the `From` half is ever delivered. The move
    /// would then decay into "the file left the repository" plus "an unrelated
    /// file appeared", which loses the metarecord: every tag, rating and note
    /// the user attached stays behind on an orphan while the file is tracked
    /// anew. The destination is found instead by the new directory's scan
    /// ([`Self::scan_dir`]), and this re-pairs the two halves.
    ///
    /// Candidates are only the paths renamed away in *this* batch that are gone
    /// from disk and still tracked; identity is settled on what a rename
    /// preserves exactly — kind, size and mtime. The fingerprint search cannot
    /// serve here: a metarecord the watcher created has no stored hashes, and
    /// its file has already left the old path, so there is nothing left to hash.
    fn find_departed_match(&mut self, rel: &RelPath) -> Result<Option<RelPath>> {
        if self.departed.is_empty() {
            return Ok(None);
        }
        let Ok(arriving) = fs_meta::stat_fields(&self.abs(rel)) else {
            return Ok(None); // Vanished mid-flush: nothing to re-pair.
        };
        let of = |name: &str| arriving.iter().find(|f| f.name == name).map(|f| &f.value);
        let Some(key) = stat_key(of("mfr_type"), of("mfr_size"), of("mfr_mtime")) else {
            return Ok(None);
        };
        self.index_departures()?;
        let candidates = match self.departed_index.as_ref().and_then(|m| m.get(&key)) {
            Some(paths) => paths.clone(),
            None => return Ok(None),
        };
        for from in candidates {
            // Still tracked? An earlier arrival in this batch may have taken it.
            if self.resolve(&from)?.is_some() {
                return Ok(Some(from));
            }
        }
        Ok(None)
    }

    /// Builds [`Apply::departed_index`] once per group: the batch's departures
    /// that are gone from disk and still tracked, keyed by their stat. One pass
    /// over the departures, so an arrival costs a hash lookup rather than a walk
    /// over all of them.
    fn index_departures(&mut self) -> Result<()> {
        if self.departed_index.is_some() {
            return Ok(());
        }
        let departed = self.departed; // a plain `&[String]`: not borrowed from `self`
        let mut index: HashMap<StatKey, Vec<RelPath>> = HashMap::new();
        for from in departed {
            // Still there: it did not leave, so it is nobody's other half.
            if std::fs::symlink_metadata(self.abs(from)).is_ok() {
                continue;
            }
            let Some(uuid) = self.resolve(from)? else {
                continue; // Untracked, or already re-homed.
            };
            let Some(record) = db::get_metarecord(self.writer.connection(), uuid)? else {
                continue;
            };
            let key =
                stat_key(record.get("mfr_type"), record.get("mfr_size"), record.get("mfr_mtime"));
            if let Some(key) = key {
                index.entry(key).or_default().push(from.clone());
            }
        }
        self.departed_index = Some(index);
        Ok(())
    }

    /// Fingerprint search among orphaned metarecords (`mfr_path` = Nothing):
    /// size pre-filter, then partial hash, then a stored full hash must
    /// confirm identity (spec watcher `Rename(To)` semantics).
    fn find_orphan_match(&mut self, abs: &Path, size: i64) -> Result<Option<Uuid>> {
        self.index_orphans()?;
        let candidates = match self.orphan_index.as_ref().and_then(|m| m.get(&size)) {
            Some(candidates) if !candidates.is_empty() => candidates.clone(),
            _ => return Ok(None),
        };
        let partial = fingerprint::partial_hash(abs)?;
        let mut full: Option<String> = None;
        for candidate in candidates {
            if candidate.partial_hash != partial {
                continue;
            }
            if full.is_none() {
                full = Some(fingerprint::full_hash(abs)?);
            }
            if full.as_deref() == Some(candidate.full_hash.as_str()) {
                // It is about to get a path again: no later arrival of this
                // flush may claim it too.
                self.forget_orphan(candidate.uuid);
                return Ok(Some(candidate.uuid));
            }
        }
        Ok(None)
    }

    /// Builds [`Apply::orphan_index`] once per group: every matchable orphan of
    /// the repository, indexed by size. One query, against one per arriving
    /// file.
    fn index_orphans(&mut self) -> Result<()> {
        if self.orphan_index.is_some() {
            return Ok(());
        }
        let mut index: HashMap<i64, Vec<db::OrphanCandidate>> = HashMap::new();
        for candidate in db::hashed_orphans(self.writer.connection())? {
            index.entry(candidate.size).or_default().push(candidate);
        }
        self.orphan_index = Some(index);
        Ok(())
    }

    /// Drops a metarecord from the orphan index: it has just been re-homed, so
    /// it is no longer an orphan.
    fn forget_orphan(&mut self, uuid: Uuid) {
        if let Some(index) = self.orphan_index.as_mut() {
            for candidates in index.values_mut() {
                candidates.retain(|c| c.uuid != uuid);
            }
        }
    }

    /// `Modify(Data)`: refresh size and mtime, invalidate the hashes.
    fn apply_modify_data(&mut self, rel: &RelPath) -> Result<()> {
        if !self.eligible(rel)? {
            return Ok(());
        }
        match self.resolve(rel)? {
            Some(uuid) => self.refresh_data(uuid, rel),
            // Modified but never tracked (e.g. lost create): treat as create.
            None => self.apply_create(rel),
        }
    }

    fn refresh_data(&mut self, uuid: Uuid, rel: &RelPath) -> Result<()> {
        let Ok(stat) = fs_meta::stat_fields_in(self.root, &self.abs(rel)) else {
            return Ok(());
        };
        let new_of = |name: &str| stat.iter().find(|f| f.name == name).map(|f| f.value.clone());
        // Idempotent: when the stored stat already matches the file and the hashes
        // are already cleared, produce no operation (no version bump). This
        // suppresses a watcher echo of a change the daemon itself just recorded —
        // e.g. a cross-repo sync file operation (spec-sync "Suppressing sync's own
        // echoes"), and a crash replay (spec-file-tracking).
        let unchanged = {
            let conn = self.writer.connection();
            let stored = |name: &str| -> Option<Value> {
                db::get_field_rows_named(conn, uuid, name).ok().and_then(|rows| {
                    rows.into_iter().map(|r| r.value).find(|v| !matches!(v, Value::Nothing))
                })
            };
            stored("mfr_size") == new_of("mfr_size")
                && stored("mfr_mtime") == new_of("mfr_mtime")
                && stored("mfr_partial_hash").is_none()
                && stored("mfr_full_hash").is_none()
        };
        if unchanged {
            return Ok(());
        }
        for field in stat {
            if matches!(field.name.as_str(), "mfr_size" | "mfr_mtime") {
                self.writer.set_field_as(OpType::FileModified, uuid, &field.name, field.value)?;
            }
        }
        self.writer.clear_field_as(OpType::FileModified, uuid, "mfr_partial_hash")?;
        self.writer.clear_field_as(OpType::FileModified, uuid, "mfr_full_hash")?;
        Ok(())
    }

    /// `Modify(Metadata)`: refresh attributes; hashes stay valid.
    fn apply_modify_meta(&mut self, rel: &RelPath) -> Result<()> {
        if !self.eligible(rel)? {
            return Ok(());
        }
        let Some(uuid) = self.resolve(rel)? else {
            return Ok(());
        };
        let Ok(stat) = fs_meta::stat_fields(&self.abs(rel)) else {
            return Ok(());
        };
        for field in stat {
            if matches!(
                field.name.as_str(),
                "mfr_permissions" | "mfr_uid" | "mfr_gid" | "mfr_mtime"
            ) {
                self.writer.set_field_as(OpType::FileModified, uuid, &field.name, field.value)?;
            }
        }
        Ok(())
    }
}

/// Resolves the parent directory entry of `rel`, creating any missing
/// intermediate directory metarecords along the way (with their stat fields and
/// `extra_fields` — e.g. `mf_watch = false` for track). Shared between the
/// executor, reconcile, and the track endpoint.
pub(crate) fn ensure_parent_metarecords(
    writer: &mut Writer,
    cache: &mut TreeCache,
    root: &Path,
    rel: &RelPath,
    extra_fields: &[Field],
) -> Result<Uuid> {
    let mut parent = cache
        .resolve_path(writer.connection(), "mfr_path", "")?
        .context("filesystem root entry missing — was the repository initialised?")?;
    // Walk down the ancestors, creating what is missing. The prefix carries the
    // exact bytes, so a directory whose own name does not decode is created
    // like any other and its children still resolve underneath it.
    let mut prefix = RelPath::root();
    for comp in rel.parent().components() {
        prefix = prefix.child(comp.clone());
        if let Some(existing) = cache.resolve_rel(writer.connection(), "mfr_path", &prefix)? {
            parent = existing;
            continue;
        }
        let mut fields = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: comp.clone() },
        )];
        match fs_meta::stat_fields_in(root, &prefix.to_abs(root)) {
            Ok(stat) => fields.extend(stat),
            // Directory already gone: minimal metarecord, reconcile fixes it.
            Err(_) => fields.push(Field::new("mfr_type", Value::String("dir".into()))),
        }
        fields.extend(extra_fields.iter().cloned());
        let created = writer.create_metarecord(fields)?;
        cache.apply_insert("mfr_path", Some(parent), comp, created.uuid);
        parent = created.uuid;
    }
    Ok(parent)
}

// ── Background executor ───────────────────────────────────────────────────────

enum ExecMsg {
    Activity,
    Shutdown,
}

/// Cloneable handle used by the watcher to signal activity.
#[derive(Clone)]
pub struct ExecutorPinger {
    tx: mpsc::Sender<ExecMsg>,
}

impl ExecutorPinger {
    pub fn ping(&self) {
        let _ = self.tx.send(ExecMsg::Activity);
    }
}

/// Background thread flushing the pending buffer after a quiet period
/// (default 500 ms) with no new activity.
pub struct ExecutorHandle {
    tx: mpsc::Sender<ExecMsg>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ExecutorHandle {
    pub fn pinger(&self) -> ExecutorPinger {
        ExecutorPinger { tx: self.tx.clone() }
    }
}

impl Drop for ExecutorHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(ExecMsg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub fn spawn(repo: &Arc<RepoState>, quiet: Duration) -> ExecutorHandle {
    // A Weak reference: the executor is owned (indirectly) by the RepoState;
    // holding an Arc here would create a cycle keeping the repository — and
    // its exclusive SQLite lock — alive forever.
    let repo = Arc::downgrade(repo);
    let (tx, rx) = mpsc::channel::<ExecMsg>();
    let join = std::thread::spawn(move || loop {
        match rx.recv() {
            Err(_) | Ok(ExecMsg::Shutdown) => return,
            Ok(ExecMsg::Activity) => loop {
                // Debounce: wait until `quiet` elapses with no new event.
                match rx.recv_timeout(quiet) {
                    Ok(ExecMsg::Activity) => continue,
                    Ok(ExecMsg::Shutdown) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let Some(repo) = repo.upgrade() else {
                            return; // Repository unloaded.
                        };
                        if let Err(err) = flush_pending(&repo) {
                            crate::diagnostics::error("executor", format!("flush failed: {err:#}"));
                        }
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            },
        }
    });
    ExecutorHandle { tx, join: Some(join) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(p: &str) -> FsEvent {
        FsEvent::RenameFrom(p.into())
    }
    fn to(p: &str) -> FsEvent {
        FsEvent::RenameTo(p.into())
    }
    fn rename(a: &str, b: &str) -> FsEvent {
        FsEvent::Rename(a.into(), b.into())
    }

    // Files moved *into* the repository from outside arrive as `RenameTo`
    // events carrying an inotify cookie whose `RenameFrom` half is not in the
    // batch — the source is outside the watch set, so nothing will ever pair
    // with them. Searching backwards for that half, per event, is quadratic in
    // the batch: the ordinary "drop a folder in" gesture then spends minutes
    // before a single database write, inside the flush's own task.
    //
    // A ratio again, not a duration: four times the events must cost about four
    // times as much, not sixteen.
    #[test]
    fn correlate_is_linear_when_nothing_pairs() {
        fn timed(n: usize) -> std::time::Duration {
            let events: Vec<(FsEvent, Option<i64>)> =
                (0..n).map(|i| (to(&format!("/in/f{i}")), Some(i as i64 + 1))).collect();
            let start = std::time::Instant::now();
            let out = correlate_renames(events);
            let elapsed = start.elapsed();
            assert_eq!(out.len(), n, "unpaired arrivals are left untouched");
            elapsed
        }

        let base = timed(4_000);
        let four_times = timed(16_000);
        assert!(
            four_times < base * 8,
            "4000 unpaired arrivals correlated in {base:?} but 16000 in {four_times:?} — \
             the pairing is quadratic in the batch",
        );
    }

    #[test]
    fn correlate_pairs_split_rename_by_cookie() {
        let out = correlate_renames(vec![(from("/a"), Some(7)), (to("/b"), Some(7))]);
        assert_eq!(out, vec![rename("/a", "/b")]);
    }

    #[test]
    fn correlate_leaves_cookieless_or_mismatched_events_untouched() {
        // No cookie: genuine boundary crossings (a file left, another arrived).
        let out = correlate_renames(vec![(from("/a"), None), (to("/b"), None)]);
        assert_eq!(out, vec![from("/a"), to("/b")]);
        // Different cookies: unrelated renames, not fused.
        let out = correlate_renames(vec![(from("/a"), Some(1)), (to("/b"), Some(2))]);
        assert_eq!(out, vec![from("/a"), to("/b")]);
    }

    #[test]
    fn correlate_pairs_each_rename_by_its_own_cookie() {
        let out = correlate_renames(vec![
            (from("/a"), Some(1)),
            (from("/c"), Some(2)),
            (to("/d"), Some(2)),
            (to("/b"), Some(1)),
        ]);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&rename("/a", "/b")));
        assert!(out.contains(&rename("/c", "/d")));
    }

    #[test]
    fn correlate_does_not_pair_a_lone_rename_to() {
        // A `To` whose cookie has no matching `From` is a real arrival.
        let out = correlate_renames(vec![(to("/b"), Some(5))]);
        assert_eq!(out, vec![to("/b")]);
    }

    // ── compact ────────────────────────────────────────────────────────────

    fn create(p: &str) -> FsEvent {
        FsEvent::Create(p.into())
    }
    fn remove(p: &str) -> FsEvent {
        FsEvent::Remove(p.into())
    }
    fn mdata(p: &str) -> FsEvent {
        FsEvent::ModifyData(p.into())
    }
    fn mmeta(p: &str) -> FsEvent {
        FsEvent::ModifyMeta(p.into())
    }

    #[test]
    fn compact_create_then_remove_cancels() {
        assert_eq!(compact(vec![create("/a"), remove("/a")]), vec![]);
    }

    #[test]
    fn compact_create_then_rename_creates_at_destination() {
        assert_eq!(compact(vec![create("/a"), rename("/a", "/b")]), vec![create("/b")]);
    }

    #[test]
    fn compact_rename_chain_collapses() {
        assert_eq!(compact(vec![rename("/a", "/b"), rename("/b", "/c")]), vec![rename("/a", "/c")]);
    }

    // A swap through a temporary name. Collapsing `a→tmp` with `tmp→b` would
    // leave `[a→b, b→a]`: a cycle, and no sequential order can apply it — one
    // metarecord holds a given tree position, so whichever move goes first
    // lands on a position the other still occupies. The hop through the
    // intermediate path is what breaks the cycle, exactly as it did on disk.
    #[test]
    fn compact_keeps_the_hop_that_breaks_a_rename_cycle() {
        assert_eq!(
            compact(vec![rename("/a", "/tmp"), rename("/b", "/a"), rename("/tmp", "/b")]),
            vec![rename("/a", "/tmp"), rename("/b", "/a"), rename("/tmp", "/b")]
        );
        // The chain still collapses when its destination is nobody's source.
        assert_eq!(
            compact(vec![rename("/a", "/tmp"), rename("/tmp", "/b"), rename("/c", "/d")]),
            vec![rename("/a", "/b"), rename("/c", "/d")]
        );
    }

    #[test]
    fn compact_collapses_repeated_modify() {
        assert_eq!(
            compact(vec![mdata("/a"), mdata("/a"), mmeta("/a"), mmeta("/a")]),
            vec![mdata("/a"), mmeta("/a")]
        );
    }

    #[test]
    fn compact_modify_after_create_is_absorbed() {
        assert_eq!(compact(vec![create("/a"), mdata("/a"), mmeta("/a")]), vec![create("/a")]);
    }

    #[test]
    fn compact_absorbs_notify_rename_triplet() {
        assert_eq!(
            compact(vec![from("/a"), to("/b"), rename("/a", "/b")]),
            vec![rename("/a", "/b")]
        );
    }

    #[test]
    fn compact_preserves_unrelated_and_a_lone_remove() {
        assert_eq!(
            compact(vec![remove("/a"), create("/b"), mdata("/c")]),
            vec![remove("/a"), create("/b"), mdata("/c")]
        );
    }

    /// The pre-refactor O(n²) linear-scan implementation, kept as the oracle the
    /// index-based [`compact`] must match byte for byte (see `compact_matches_reference`).
    fn compact_reference(events: Vec<FsEvent>) -> Vec<FsEvent> {
        let mut out: Vec<Option<FsEvent>> = Vec::with_capacity(events.len());
        for ev in events {
            let find_last = |out: &Vec<Option<FsEvent>>, pred: &dyn Fn(&FsEvent) -> bool| {
                out.iter().rposition(|e| e.as_ref().is_some_and(pred))
            };
            match ev {
                FsEvent::Remove(p) => {
                    if let Some(i) =
                        find_last(&out, &|e| matches!(e, FsEvent::Create(q) if *q == p))
                    {
                        out[i] = None;
                    } else {
                        out.push(Some(FsEvent::Remove(p)));
                    }
                }
                FsEvent::Rename(a, b) => {
                    if let Some(i) =
                        find_last(&out, &|e| matches!(e, FsEvent::RenameFrom(q) if *q == a))
                    {
                        out[i] = None;
                    }
                    if let Some(i) =
                        find_last(&out, &|e| matches!(e, FsEvent::RenameTo(q) if *q == b))
                    {
                        out[i] = None;
                    }
                    if let Some(i) =
                        find_last(&out, &|e| matches!(e, FsEvent::Create(q) if *q == a))
                    {
                        out[i] = Some(FsEvent::Create(b));
                    } else if let Some(i) =
                        find_last(&out, &|e| matches!(e, FsEvent::Rename(_, q) if *q == a))
                            // The cycle guard: not when a rename still to be
                            // applied (a later index) starts from `b`.
                            .filter(|&i| {
                                !out.iter().skip(i + 1).any(
                                    |e| matches!(e, Some(FsEvent::Rename(src, _)) if *src == b),
                                )
                            })
                    {
                        let Some(FsEvent::Rename(x, _)) = out[i].clone() else { unreachable!() };
                        out[i] = Some(FsEvent::Rename(x, b));
                    } else {
                        out.push(Some(FsEvent::Rename(a, b)));
                    }
                }
                FsEvent::ModifyData(p) => {
                    let redundant = find_last(&out, &|e| {
                        matches!(e, FsEvent::Create(q) if *q == p)
                            || matches!(e, FsEvent::ModifyData(q) if *q == p)
                    })
                    .is_some();
                    if !redundant {
                        out.push(Some(FsEvent::ModifyData(p)));
                    }
                }
                FsEvent::ModifyMeta(p) => {
                    let redundant = find_last(&out, &|e| {
                        matches!(e, FsEvent::Create(q) if *q == p)
                            || matches!(e, FsEvent::ModifyMeta(q) if *q == p)
                    })
                    .is_some();
                    if !redundant {
                        out.push(Some(FsEvent::ModifyMeta(p)));
                    }
                }
                other => out.push(Some(other)),
            }
        }
        out.into_iter().flatten().collect()
    }

    /// Deterministic fuzz: thousands of random event streams over a tiny path
    /// alphabet, asserting the index-based `compact` equals the reference. A
    /// small alphabet maximises collisions (the interesting compaction cases).
    #[test]
    fn compact_matches_reference() {
        // A dependency-free LCG (numerical recipes constants).
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = |n: u64| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) % n
        };
        let paths = ["/a", "/b", "/c"];
        for _ in 0..50_000 {
            let len = next(13) as usize; // 0..=12 events
            let mut events = Vec::with_capacity(len);
            for _ in 0..len {
                let p = RelPath::from(paths[next(paths.len() as u64) as usize]);
                let q = RelPath::from(paths[next(paths.len() as u64) as usize]);
                events.push(match next(7) {
                    0 => FsEvent::Create(p),
                    1 => FsEvent::Remove(p),
                    2 => FsEvent::Rename(p, q),
                    3 => FsEvent::RenameFrom(p),
                    4 => FsEvent::RenameTo(p),
                    5 => FsEvent::ModifyData(p),
                    _ => FsEvent::ModifyMeta(p),
                });
            }
            assert_eq!(
                compact(events.clone()),
                compact_reference(events.clone()),
                "divergence on {events:?}"
            );
        }
    }
}
