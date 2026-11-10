//! Content hashes computed on demand (doc "Content hashes kept fresh"): the
//! partial and full hash of a set of files, with the `stat` stamp they were
//! taken under, reusing a stored hash its stamp still vouches for (doc "The
//! duplicate hash cache").

use std::collections::HashMap;
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
        for (uuid, h) in items.by_ref() {
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

// ── Keeping marked files' hashes fresh ────────────────────────────────────────

/// The files waiting for the background worker to hash them (doc "Content
/// hashes kept fresh"): each once, in the order they were handed over. In
/// memory, like the watcher's buffer: a restart loses it, and a file left
/// without a hash has its bytes compared by sync until reconcile hands it over
/// again.
#[derive(Default)]
pub struct HashQueue {
    inner: std::sync::Mutex<QueueInner>,
    /// Wakes the worker thread, once it runs.
    wake: std::sync::Mutex<Option<std::sync::mpsc::Sender<WorkerMsg>>>,
}

#[derive(Default)]
struct QueueInner {
    order: std::collections::VecDeque<Uuid>,
    queued: std::collections::HashSet<Uuid>,
}

impl HashQueue {
    /// Hands files over, and wakes the worker.
    pub fn push(&self, uuids: impl IntoIterator<Item = Uuid>) {
        let mut pushed = false;
        {
            let mut inner = self.inner.lock_recover();
            for uuid in uuids {
                if inner.queued.insert(uuid) {
                    inner.order.push_back(uuid);
                    pushed = true;
                }
            }
        }
        if pushed {
            if let Some(tx) = self.wake.lock_recover().as_ref() {
                let _ = tx.send(WorkerMsg::Wake);
            }
        }
    }

    fn pop(&self) -> Option<Uuid> {
        let mut inner = self.inner.lock_recover();
        let uuid = inner.order.pop_front()?;
        inner.queued.remove(&uuid);
        Some(uuid)
    }

    /// How many files wait.
    pub fn len(&self) -> usize {
        self.inner.lock_recover().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What the worker learns from its file, under the lock: where it is, and the
/// stat its record holds.
struct Job {
    uuid: Uuid,
    rel: RelPath,
    /// The record's `(mfr_mtime, mfr_size)`: what a hash must be stamped with
    /// to be fresh.
    recorded: (i64, i64),
}

/// The record's `(mfr_mtime, mfr_size)`, when it holds both.
fn recorded_stat(conn: &dyn crate::store::Store, uuid: Uuid) -> anyhow::Result<Option<(i64, i64)>> {
    let mtime =
        Rows::rows_named(conn, uuid, "mfr_mtime")?.into_iter().find_map(|r| match r.value {
            Value::DateTime(ms) => Some(ms),
            _ => None,
        });
    Ok(mtime.zip(Rows::int_field(conn, uuid, "mfr_size")?))
}

/// Reads what hashing `uuid` needs, or `None` when there is nothing to do: a
/// record gone, no longer a marked file, or with no path.
fn job_for(
    conn: &dyn crate::store::Store,
    cache: &crate::forest::Forest,
    uuid: Uuid,
) -> anyhow::Result<Option<Job>> {
    if Rows::version(conn, uuid)?.is_none() {
        return Ok(None);
    }
    if Rows::string_fields(conn, uuid, "mfr_type")?.first().map(String::as_str) != Some("file") {
        return Ok(None);
    }
    if !crate::eligibility::mf_hash_of(conn, uuid)? {
        return Ok(None);
    }
    let (Some(rel), Some(recorded)) = (cache.rel_path_of(conn, uuid)?, recorded_stat(conn, uuid)?)
    else {
        return Ok(None);
    };
    Ok(Some(Job { uuid, rel, recorded }))
}

/// The marked files — of `scope`'s subtree, or the whole repository — whose
/// full hash is missing or not fresh: no stored hash stamped with the stat
/// their record holds. What reconcile hands over to the worker. Nothing is
/// read past the `mf_hash` rows when no record sets the field.
pub fn needing_hash(
    conn: &dyn crate::store::Store,
    cache: &crate::forest::Forest,
    scope: Option<Uuid>,
) -> anyhow::Result<Vec<Uuid>> {
    if !Rows::field_rows(conn, "mf_hash")?.iter().any(|(_, r)| r.value == Value::Bool(true)) {
        return Ok(Vec::new());
    }
    let files: Vec<Uuid> = match scope {
        None => crate::store::Questions::tracked_files_with_size(conn)?
            .into_iter()
            .map(|(uuid, _)| uuid)
            .collect(),
        Some(root) => {
            let mut all = cache.descendants(conn, "mfr_path", root)?;
            all.push(root);
            let mut files = Vec::new();
            for uuid in all {
                if Rows::string_fields(conn, uuid, "mfr_type")?.first().map(String::as_str)
                    == Some("file")
                {
                    files.push(uuid);
                }
            }
            files
        }
    };
    let mut marks = MarkMemo::default();
    let mut out = Vec::new();
    for uuid in files {
        if !marks.marked(conn, uuid)? {
            continue;
        }
        let stored_full = Rows::string_fields(conn, uuid, "mfr_full_hash")?;
        let stamp = Rows::rows_named(conn, uuid, "mfr_hash_mtime")?
            .into_iter()
            .find_map(|r| match r.value {
                Value::DateTime(ms) => Some(ms),
                _ => None,
            })
            .zip(Rows::int_field(conn, uuid, "mfr_hash_size")?);
        let fresh =
            !stored_full.is_empty() && stamp.is_some() && stamp == recorded_stat(conn, uuid)?;
        if !fresh {
            out.push(uuid);
        }
    }
    Ok(out)
}

/// [`crate::eligibility::mf_hash_of`] over many records: each ancestor's
/// answer is kept, so a whole tree costs one read per node rather than one walk
/// per file.
#[derive(Default)]
struct MarkMemo(HashMap<Uuid, bool>);

impl MarkMemo {
    fn marked(&mut self, conn: &dyn crate::store::Store, uuid: Uuid) -> anyhow::Result<bool> {
        let mut chain = Vec::new();
        let mut cur = Some(uuid);
        let mut answer = false;
        for _ in 0..crate::log::MAX_TREE_DEPTH {
            let Some(node) = cur else { break };
            if let Some(&known) = self.0.get(&node) {
                answer = known;
                break;
            }
            chain.push(node);
            if let Some(own) = Rows::bool_field(conn, node, "mf_hash")? {
                answer = own;
                break;
            }
            cur = Rows::rows_named(conn, node, "mfr_path")?
                .into_iter()
                .find_map(|r| r.value.position().map(|(p, _)| p))
                .flatten();
        }
        for node in chain {
            self.0.insert(node, answer);
        }
        Ok(answer)
    }
}

/// How long the worker's computed hashes wait before it writes them.
const WORKER_COMMIT_EVERY: Duration = Duration::from_secs(1);

/// Hashes every file waiting in the queue, and writes each hash whose stamp
/// is the stat its record holds — a record that has not caught up with its
/// file yet is left to the event that will hand the file over again. The
/// hashing itself runs with the store free. Returns how many files got a hash
/// written. What the worker thread runs; tests call it directly.
pub fn drain(repo: &RepoState) -> anyhow::Result<usize> {
    drain_while(repo, || true)
}

/// [`drain`], leaving the queue after the file in hand as soon as `keep_going`
/// says so — the worker stops once the repository is being unloaded.
fn drain_while(repo: &RepoState, keep_going: impl Fn() -> bool) -> anyhow::Result<usize> {
    let root = repo.config.root.clone();
    let mut context = crate::log::current_context().named("hash");
    let mut pending: Vec<(Uuid, Hashes)> = Vec::new();
    let mut written = 0usize;
    let mut opened = Instant::now();
    loop {
        if !keep_going()
            || repo.is_revert_locked()
            || !repo.root_present()
            || repo.ensure_writable().is_err()
        {
            // Left queued: the next hand-over wakes the worker again.
            break;
        }
        let Some(uuid) = repo.hash_queue.pop() else { break };
        let job = {
            let conn = metafolder_core::slowlog::timed("wait:conn", || repo.conn.lock_recover());
            job_for(&*conn, &repo.tree(), uuid)?
        };
        let Some(job) = job else { continue };
        // Always read afresh: the worker is handed a file because its content
        // may have changed under a stamp that did not move.
        if let Outcome::Computed(h) = hash_file(&job.rel.to_abs(&root), None, true) {
            if h.stamp == job.recorded {
                pending.push((job.uuid, h));
            }
        }
        if opened.elapsed() >= WORKER_COMMIT_EVERY {
            written += write_fresh(repo, &mut pending, &mut context)?;
            opened = Instant::now();
        }
    }
    written += write_fresh(repo, &mut pending, &mut context)?;
    Ok(written)
}

/// Writes the worker's hashes, each only if its record still holds the stat it
/// was stamped with. Returns how many were written.
fn write_fresh(
    repo: &RepoState,
    pending: &mut Vec<(Uuid, Hashes)>,
    context: &mut WriteContext,
) -> anyhow::Result<usize> {
    if pending.is_empty() {
        return Ok(0);
    }
    let mut conn = metafolder_core::slowlog::timed("wait:conn", || repo.conn.lock_recover());
    let mut writer = repo.writer(&mut *conn, context.clone())?;
    let mut written = 0;
    for (uuid, h) in std::mem::take(pending) {
        if recorded_stat(writer.store(), uuid)? == Some(h.stamp) {
            write_hashes(&mut writer, uuid, &h)?;
            written += 1;
        }
    }
    if writer.op_count() > 0 {
        *context = WriteContext::In(writer.label_id());
    }
    writer.commit()?;
    Ok(written)
}

enum WorkerMsg {
    Wake,
    Shutdown,
}

/// The background worker's thread; dropping it stops the thread once the file
/// in hand is done.
pub struct WorkerHandle {
    tx: std::sync::mpsc::Sender<WorkerMsg>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerMsg::Shutdown);
        if let Some(join) = self.join.take() {
            // The worker itself may hold the last reference to the repository,
            // and drop it here: it cannot wait for its own end.
            if join.thread().id() != std::thread::current().id() {
                let _ = join.join();
            }
        }
    }
}

/// Starts the worker of `repo`: it drains the queue each time files are handed
/// over. A Weak reference, like the executor's: the repository owns the
/// handle, and an `Arc` here would keep it alive forever.
pub fn spawn_worker(repo: &std::sync::Arc<RepoState>) -> WorkerHandle {
    let (tx, rx) = std::sync::mpsc::channel::<WorkerMsg>();
    *repo.hash_queue.wake.lock_recover() = Some(tx.clone());
    let weak = std::sync::Arc::downgrade(repo);
    let join = std::thread::spawn(move || loop {
        match rx.recv() {
            Err(_) | Ok(WorkerMsg::Shutdown) => return,
            Ok(WorkerMsg::Wake) => {
                let Some(repo) = weak.upgrade() else { return };
                let _scope = crate::diagnostics::in_repo(repo.uuid());
                // Only while someone else holds the repository: once it is
                // unloaded, this reference is the last, and the queue is left.
                let held = || weak.strong_count() > 1;
                if let Err(err) = drain_while(&repo, held) {
                    crate::diagnostics::error_for(
                        "hashing",
                        format!("hashing failed: {err:#}"),
                        repo.uuid(),
                    );
                }
            }
        }
    });
    // Whatever was handed over before the thread existed.
    if !repo.hash_queue.is_empty() {
        let _ = tx.send(WorkerMsg::Wake);
    }
    WorkerHandle { tx, join: Some(join) }
}
