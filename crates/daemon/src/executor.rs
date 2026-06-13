//! Pending-event executor (spec-file-tracking "Event batching"): the watcher
//! enqueues raw filesystem events into the persistent `pending_operation`
//! table; after a quiet period the executor compacts them, groups them by
//! resulting operation type (one revision per group), and applies the event
//! semantics to the data tables through the logged write flow.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use rusqlite::{params, Connection};
use uuid::Uuid;

use metafolder_core::metarecord::{Field, Value};

use crate::db;
use crate::eligibility;
use crate::fingerprint;
use crate::fs_meta;
use crate::log::{OpType, Writer};
use crate::state::RepoState;
use crate::tree_cache::TreeCache;

/// A raw filesystem event, as enqueued by the watcher. Paths are
/// repo-root-relative with a leading `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    Create(String),
    Remove(String),
    /// Correlated rename: both paths are inside the repository.
    Rename(String, String),
    /// The file definitively left the repository.
    RenameFrom(String),
    /// The file arrived from outside the repository.
    RenameTo(String),
    ModifyData(String),
    ModifyMeta(String),
}

/// Appends one event to the persistent buffer.
pub fn enqueue(conn: &Connection, ev: &FsEvent) -> Result<()> {
    let (op_type, path, from, to): (&str, Option<&str>, Option<&str>, Option<&str>) = match ev {
        FsEvent::Create(p) => ("fs_create", Some(p), None, None),
        FsEvent::Remove(p) => ("fs_remove", Some(p), None, None),
        FsEvent::Rename(a, b) => ("fs_rename", None, Some(a), Some(b)),
        FsEvent::RenameFrom(p) => ("fs_rename_from", Some(p), None, None),
        FsEvent::RenameTo(p) => ("fs_rename_to", Some(p), None, None),
        FsEvent::ModifyData(p) => ("fs_modify_data", Some(p), None, None),
        FsEvent::ModifyMeta(p) => ("fs_modify_meta", Some(p), None, None),
    };
    conn.execute(
        "INSERT INTO pending_operation (op_type, path, from_path, to_path)
         VALUES (?1, ?2, ?3, ?4)",
        params![op_type, path, from, to],
    )?;
    Ok(())
}

fn load_pending(conn: &Connection) -> Result<(Vec<FsEvent>, i64)> {
    let mut stmt = conn.prepare(
        "SELECT id, op_type, path, from_path, to_path FROM pending_operation
         WHERE op_type LIKE 'fs_%' ORDER BY id",
    )?;
    let mut events = Vec::new();
    let mut max_id = 0;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    for row in rows {
        let (id, op, path, from, to) = row?;
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
        events.push(ev);
    }
    Ok((events, max_id))
}

/// Compaction rules of spec-file-tracking: redundant sequences within the
/// batching window are simplified before any database write.
fn compact(events: Vec<FsEvent>) -> Vec<FsEvent> {
    let mut out: Vec<Option<FsEvent>> = Vec::with_capacity(events.len());
    for ev in events {
        let find_last = |out: &Vec<Option<FsEvent>>, pred: &dyn Fn(&FsEvent) -> bool| {
            out.iter().rposition(|e| e.as_ref().is_some_and(pred))
        };
        match ev {
            FsEvent::Remove(p) => {
                // Create A, Remove A → nothing.
                if let Some(i) =
                    find_last(&out, &|e| matches!(e, FsEvent::Create(q) if *q == p))
                {
                    out[i] = None;
                } else {
                    out.push(Some(FsEvent::Remove(p)));
                }
            }
            FsEvent::Rename(a, b) => {
                // The notify backend emits Rename(From) + Rename(To) and
                // then the correlated Rename(Both) for the same move: the
                // one-sided pair is absorbed by the Both event.
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
                // Create A, Rename A→B → Create B.
                if let Some(i) =
                    find_last(&out, &|e| matches!(e, FsEvent::Create(q) if *q == a))
                {
                    out[i] = Some(FsEvent::Create(b));
                }
                // Rename X→A, Rename A→B → Rename X→B.
                else if let Some(i) =
                    find_last(&out, &|e| matches!(e, FsEvent::Rename(_, q) if *q == a))
                {
                    let Some(FsEvent::Rename(x, _)) = out[i].clone() else { unreachable!() };
                    out[i] = Some(FsEvent::Rename(x, b));
                } else {
                    out.push(Some(FsEvent::Rename(a, b)));
                }
            }
            FsEvent::ModifyData(p) => {
                // Create A, Modify A → Create A; Modify ×N → one Modify.
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
}

/// Processes the whole pending buffer: compaction, grouping, application.
/// Also used at load time to replay a buffer left by a previous daemon run.
pub fn flush_pending(repo: &RepoState) -> Result<FlushStats> {
    // While a coordinated rollback holds the lock, pending operations (watcher
    // events and restoration ops) accumulate but are not committed; they are
    // replayed once the lock is released (spec-event-log "Rollback lock").
    if repo.is_rollback_locked() {
        return Ok(FlushStats::default());
    }
    let mut conn = repo.conn.lock().unwrap();
    let mut cache = repo.cache.lock().unwrap();

    // Restoration ops from skipped rollback steps are replayed first, as their
    // own revision, before the watcher events recorded during the lock.
    let mut revisions_from_restore = 0;
    revisions_from_restore += flush_restorations(&mut conn, &mut cache, repo.config.repo_uuid)?;

    let (events, max_id) = load_pending(&conn)?;
    if events.is_empty() {
        return Ok(FlushStats { events: 0, revisions: revisions_from_restore });
    }
    let events = compact(events);
    let n_events = events.len();

    // Group by kind, keeping groups ordered by first occurrence.
    let mut groups: Vec<(GroupKind, Vec<FsEvent>)> = Vec::new();
    for ev in events {
        let kind = group_kind(&ev);
        match groups.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, list)) => list.push(ev),
            None => groups.push((kind, vec![ev])),
        }
    }

    let mut revisions = 0;
    for (_, group) in groups {
        let writer = Writer::begin(&mut conn, repo.config.repo_uuid, None)?;
        let mut apply = Apply {
            writer,
            cache: &mut cache,
            root: &repo.config.root,
            db_id: repo.config.repo_uuid,
        };
        for ev in group {
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
    Ok(FlushStats { events: n_events, revisions: revisions + revisions_from_restore })
}

/// Replays restoration ops left by skipped coordinated-rollback steps as a
/// single revision (spec-event-log "skip"), then deletes them. The tree cache
/// is cleared afterwards because `mfr_path` restorations move tree positions.
fn flush_restorations(conn: &mut Connection, cache: &mut TreeCache, db_id: Uuid) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, op_type, path, from_path, to_path FROM pending_operation
         WHERE op_type LIKE 'restore_%' ORDER BY id",
    )?;
    // (id, op_type, path, from_path, to_path)
    type RestoreRow = (i64, String, Option<String>, Option<String>, Option<String>);
    let rows: Vec<RestoreRow> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    if rows.is_empty() {
        return Ok(0);
    }
    let max_id = rows.iter().map(|r| r.0).max().unwrap_or(0);

    let parse_uuid = |s: &str| -> Result<Uuid> {
        Uuid::parse_str(s).with_context(|| format!("invalid uuid in restoration op: {s}"))
    };

    let mut writer = Writer::begin(conn, db_id, None)?;
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
                    Value::TreeRef { parent, name },
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
    cache.clear();
    conn.execute("DELETE FROM pending_operation WHERE id <= ?1 AND op_type LIKE 'restore_%'", params![max_id])?;
    Ok(if wrote { 1 } else { 0 })
}

/// Application context for one revision (one group of events).
struct Apply<'a, 'c> {
    writer: Writer<'c>,
    cache: &'a mut TreeCache,
    root: &'a Path,
    db_id: Uuid,
}

impl Apply<'_, '_> {
    fn apply(&mut self, ev: FsEvent) -> Result<()> {
        match ev {
            FsEvent::Create(p) => self.apply_create(&p),
            FsEvent::Remove(p) | FsEvent::RenameFrom(p) => self.apply_remove(&p),
            FsEvent::Rename(a, b) => self.apply_rename(&a, &b),
            FsEvent::RenameTo(p) => self.apply_arrival(&p),
            FsEvent::ModifyData(p) => self.apply_modify_data(&p),
            FsEvent::ModifyMeta(p) => self.apply_modify_meta(&p),
        }
    }

    fn abs(&self, rel: &str) -> std::path::PathBuf {
        self.root.join(rel.trim_start_matches('/'))
    }

    fn eligible(&mut self, rel: &str) -> Result<bool> {
        eligibility::is_eligible(self.writer.connection(), self.cache, rel)
    }

    fn resolve(&mut self, rel: &str) -> Result<Option<Uuid>> {
        self.cache.resolve_path(self.writer.connection(), "mfr_path", rel)
    }

    /// Splits "/a/b/name" into ("/a/b", "name").
    fn split_parent(rel: &str) -> (&str, &str) {
        match rel.rfind('/') {
            Some(i) => (&rel[..i], &rel[i + 1..]),
            None => ("", rel),
        }
    }

    /// Resolves the parent directory entry of `rel`, creating any missing
    /// intermediate directory metarecords (with their stat fields).
    fn ensure_parents(&mut self, rel: &str) -> Result<Uuid> {
        ensure_parent_metarecords(&mut self.writer, self.cache, self.root, rel, &[])
    }

    fn apply_create(&mut self, rel: &str) -> Result<()> {
        if !self.eligible(rel)? {
            return Ok(());
        }
        if let Some(existing) = self.resolve(rel)? {
            // Already tracked (e.g. replay after crash): refresh instead.
            return self.refresh_data(existing, rel);
        }
        let Ok(stat) = fs_meta::stat_fields(&self.abs(rel)) else {
            return Ok(()); // The file disappeared before the flush.
        };
        let parent = self.ensure_parents(rel)?;
        let (_, name) = Self::split_parent(rel);
        let mut fields = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: name.to_string() },
        )];
        fields.extend(stat);
        let created = self.writer.create_metarecord(fields)?;
        self.cache.apply_insert("mfr_path", Some(parent), name, created.uuid);
        Ok(())
    }

    /// `Remove` / `Rename(From)`: the metarecord is preserved, `mfr_path` becomes
    /// Nothing, and the whole subtree is cleared in the same transaction.
    fn apply_remove(&mut self, rel: &str) -> Result<()> {
        let Some(uuid) = self.resolve(rel)? else {
            return Ok(());
        };
        if !self.eligible(rel)? {
            return Ok(()); // Out of watch scope: metadata left unchanged.
        }
        let descendants =
            self.cache.descendants(self.writer.connection(), "mfr_path", uuid)?;
        self.writer.set_field_as(OpType::FileDeleted, uuid, "mfr_path", Value::Nothing)?;
        for descendant in descendants {
            self.writer
                .set_field_as(OpType::FileDeleted, descendant, "mfr_path", Value::Nothing)?;
        }
        self.cache.apply_remove("mfr_path", uuid);
        Ok(())
    }

    fn apply_rename(&mut self, from: &str, to: &str) -> Result<()> {
        let Some(src) = self.resolve(from)? else {
            // Unknown source: treat as an arrival at the destination.
            return self.apply_arrival(to);
        };
        if !self.eligible(to)? {
            return Ok(()); // Moved out of scope: keep the stale path.
        }
        let parent = self.ensure_parents(to)?;
        let (_, name) = Self::split_parent(to);
        self.writer.set_field_as(
            OpType::FileMoved,
            src,
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: name.to_string() },
        )?;
        self.cache.apply_rename("mfr_path", src, Some(parent), name);
        Ok(())
    }

    /// `Rename(To)`: the file arrived from outside. Reuse an orphaned metarecord
    /// when a full-hash fingerprint confirms identity, otherwise create.
    fn apply_arrival(&mut self, rel: &str) -> Result<()> {
        if !self.eligible(rel)? {
            return Ok(());
        }
        if let Some(existing) = self.resolve(rel)? {
            return self.refresh_data(existing, rel);
        }
        let abs = self.abs(rel);
        let Ok(meta) = std::fs::metadata(&abs) else {
            return Ok(());
        };
        if meta.is_file() {
            if let Some(orphan) = self.find_orphan_match(&abs, meta.len() as i64)? {
                let parent = self.ensure_parents(rel)?;
                let (_, name) = Self::split_parent(rel);
                self.writer.set_field_as(
                    OpType::FileMoved,
                    orphan,
                    "mfr_path",
                    Value::TreeRef { parent: Some(parent), name: name.to_string() },
                )?;
                self.cache.apply_insert("mfr_path", Some(parent), name, orphan);
                // Refresh the stat-derived fields at the new location.
                for field in fs_meta::stat_fields(&abs)? {
                    self.writer.set_field_as(
                        OpType::FileModified,
                        orphan,
                        &field.name,
                        field.value,
                    )?;
                }
                return Ok(());
            }
        }
        self.apply_create(rel)
    }

    /// Fingerprint search among orphaned metarecords (`mfr_path` = Nothing):
    /// size pre-filter, then partial hash, then a stored full hash must
    /// confirm identity (spec watcher `Rename(To)` semantics).
    fn find_orphan_match(&mut self, abs: &Path, size: i64) -> Result<Option<Uuid>> {
        let candidates = db::find_orphans_by_size(self.writer.connection(), self.db_id, size)?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let partial = fingerprint::partial_hash(abs)?;
        let mut full: Option<String> = None;
        for candidate in candidates {
            let conn = self.writer.connection();
            let stored_partial = string_field(conn, candidate, "mfr_partial_hash")?;
            let stored_full = string_field(conn, candidate, "mfr_full_hash")?;
            let (Some(stored_partial), Some(stored_full)) = (stored_partial, stored_full) else {
                continue; // Without a stored full hash, identity cannot be confirmed.
            };
            if stored_partial != partial {
                continue;
            }
            if full.is_none() {
                full = Some(fingerprint::full_hash(abs)?);
            }
            if full.as_deref() == Some(stored_full.as_str()) {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    /// `Modify(Data)`: refresh size and mtime, invalidate the hashes.
    fn apply_modify_data(&mut self, rel: &str) -> Result<()> {
        if !self.eligible(rel)? {
            return Ok(());
        }
        match self.resolve(rel)? {
            Some(uuid) => self.refresh_data(uuid, rel),
            // Modified but never tracked (e.g. lost create): treat as create.
            None => self.apply_create(rel),
        }
    }

    fn refresh_data(&mut self, uuid: Uuid, rel: &str) -> Result<()> {
        let Ok(stat) = fs_meta::stat_fields(&self.abs(rel)) else {
            return Ok(());
        };
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
    fn apply_modify_meta(&mut self, rel: &str) -> Result<()> {
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
    rel: &str,
    extra_fields: &[Field],
) -> Result<Uuid> {
    let parent_path = match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    };
    let comps: Vec<&str> = parent_path.split('/').collect();
    let mut parent = cache
        .resolve_path(writer.connection(), "mfr_path", "")?
        .context("filesystem root entry missing — was the repository initialised?")?;
    let mut prefix = String::new();
    for comp in comps.iter().skip(1) {
        prefix.push('/');
        prefix.push_str(comp);
        if let Some(existing) = cache.resolve_path(writer.connection(), "mfr_path", &prefix)? {
            parent = existing;
            continue;
        }
        let mut fields = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: comp.to_string() },
        )];
        match fs_meta::stat_fields(&root.join(prefix.trim_start_matches('/'))) {
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

fn string_field(conn: &Connection, uuid: Uuid, name: &str) -> Result<Option<String>> {
    Ok(db::get_field_rows_named(conn, uuid, name)?
        .into_iter()
        .find_map(|r| match r.value {
            Value::String(s) => Some(s),
            _ => None,
        }))
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
                            eprintln!("[executor] flush failed: {err:#}");
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
