//! The logged write flow (doc "Log storage"). Every write to
//! the data tables goes through a [`Writer`], which records, under a label
//! (doc "Labels and contexts"), one operation per atomic change with before/after snapshots, and keeps the
//! `head` pointer (the newest operation) consistent with the data tables — all
//! in one store transaction. The log is a sequence, not a tree: nothing moves
//! back over it (doc "The operation log").

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use roaring::RoaringBitmap;
use uuid::Uuid;

pub use metafolder_core::date::now_ms;
use metafolder_core::metarecord::{Field, FieldType, MetaRecord, Value};

use crate::error::DomainError;
use crate::rows::{self, FieldRow};
use crate::store::{Begin, Database, LabelMeta, Log, MassKind, NewOp, Store, WriteTxn};

/// The `origin` of an operation the daemon writes on the filesystem's
/// behalf — the watcher's flush (doc "Operations and their origin").
/// A client's own write has none.
pub const ORIGIN_WATCHER: &str = "watcher";

/// The `origin` of the metarecord deletion a trashing writes
/// (doc "POST /repos/:repo/metarecords/trash").
///
/// Deliberately *not* `watcher`: a trashing is a write the user asked for and
/// must stay undoable, and only `watcher` disqualifies an operation from being
/// undone (doc "Operations and their origin"). It is a distinct value only so
/// that a client reverting it knows the bytes of a deleted metarecord are
/// in the trash-bin rather than gone.
pub const ORIGIN_TRASH: &str = "trash";

/// Maximum depth of a TreeRef chain (doc "TreeRef forest").
pub const MAX_TREE_DEPTH: usize = 1000;

/// Operation types recorded in the log (doc "Operations and their origin").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpType {
    CreateRecord,
    DeleteRecord,
    SetRecord,
    SetField,
    AppendField,
    DeleteField,
    FileDeleted,
    FileMoved,
    FileModified,
    Unknown,
}

impl OpType {
    pub fn as_str(self) -> &'static str {
        match self {
            OpType::CreateRecord => "create_metarecord",
            OpType::DeleteRecord => "delete_metarecord",
            OpType::SetRecord => "set_metarecord",
            OpType::SetField => "set_field",
            OpType::AppendField => "append_field",
            OpType::DeleteField => "delete_field",
            OpType::FileDeleted => "file_deleted",
            OpType::FileMoved => "file_moved",
            OpType::FileModified => "file_modified",
            OpType::Unknown => "unknown",
        }
    }

    /// Whether the operation is a *manual* write — something a client asked
    /// for — as opposed to one the watcher records on the filesystem's behalf.
    ///
    /// The filesystem is authoritative for `mfr_path`: a directory that is gone
    /// is gone, and the watcher nulls its whole subtree (doc "Event semantics").
    /// A manual write has no such story, so it is held to
    /// the forest's referential integrity instead (see
    /// `Writer::check_forest_integrity`).
    pub fn is_manual(self) -> bool {
        !matches!(self, OpType::FileDeleted | OpType::FileMoved | OpType::FileModified)
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "create_metarecord" => OpType::CreateRecord,
            "delete_metarecord" => OpType::DeleteRecord,
            "set_metarecord" => OpType::SetRecord,
            "set_field" => OpType::SetField,
            "append_field" => OpType::AppendField,
            "delete_field" => OpType::DeleteField,
            "file_deleted" => OpType::FileDeleted,
            "file_moved" => OpType::FileMoved,
            "file_modified" => OpType::FileModified,
            "unknown" => OpType::Unknown,
            _ => return None,
        })
    }
}

// ── History reading ───────────────────────────────────────────────────────────

/// One row of the `operation` table.
#[derive(Debug, Clone)]
pub struct OpRow {
    pub id: i64,
    /// Its innermost label (doc "Labels and contexts").
    pub label_id: i64,
    /// When it was written, Unix milliseconds.
    pub timestamp: i64,
    pub op_type: String,
    pub entity_uuid: Uuid,
    pub field_name: Option<String>,
    /// The operation this one undid, when a revert wrote it (doc "Revert").
    /// `None` for an ordinary write — and for every row of a
    /// database written before the column existed. Allowed to dangle: pruning
    /// may remove the operation it names.
    pub reverts_op_id: Option<i64>,
    /// Who wrote it (doc "Operations and their origin"): [`ORIGIN_WATCHER`],
    /// [`ORIGIN_TRASH`], or `None` for a client's ordinary write.
    ///
    /// It is what separates two operations the op type alone cannot: an
    /// ordinary `delete_metarecord` touches no file, while the one a *trashing*
    /// wrote has its bytes waiting in the trash-bin.
    pub origin: Option<String>,
}

// ── Contexts (doc "Labels and contexts") ────────────────────────────────────

/// Where a write's operations go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteContext {
    /// Under a label that exists already: the context a client passed.
    In(i64),
    /// Under a label the write opens — and drops again when it writes
    /// nothing.
    New(NewLabel),
    /// Under the label of a client's command: opened at the command's first
    /// write (inside `label.parent`), found again by `key` at its next ones.
    Command { key: Uuid, label: NewLabel },
    /// Under a label opened for one flush of the watcher, inside the
    /// watcher's own label. The operations' origin is [`ORIGIN_WATCHER`].
    Watcher,
}

/// The command a request says it is a step of (`X-Metafolder-Command` and
/// its name and description, doc "Labels and contexts").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHeader {
    pub key: Uuid,
    pub name: String,
    pub description: Option<String>,
}

/// A label a write opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLabel {
    pub name: String,
    pub description: Option<String>,
    pub parent: Option<i64>,
}

impl WriteContext {
    /// A new root label named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        WriteContext::New(NewLabel { name: name.into(), description: None, parent: None })
    }

    /// The context of a request: the label of the command it is a step of
    /// (inside the label the client named), or inside the label the client
    /// named, or a new root label named `name` when it named neither.
    pub fn of_request(
        label: Option<i64>,
        command: Option<CommandHeader>,
        name: impl Into<String>,
    ) -> Self {
        match (command, label) {
            (Some(command), parent) => WriteContext::Command {
                key: command.key,
                label: NewLabel { name: command.name, description: command.description, parent },
            },
            (None, Some(id)) => WriteContext::In(id),
            (None, None) => WriteContext::new(name),
        }
    }

    /// The same context, its new label (if it opens one) named `name`
    /// instead.
    pub fn named(self, name: impl Into<String>) -> Self {
        match self {
            WriteContext::New(label) => WriteContext::New(NewLabel { name: name.into(), ..label }),
            other => other,
        }
    }
}

std::thread_local! {
    static CONTEXT: std::cell::RefCell<Option<WriteContext>> =
        const { std::cell::RefCell::new(None) };
}

/// The context the work running on this thread writes in: the request's
/// (set by the routes for the length of its work, and by a task spawned off
/// one), or a label named "daemon" outside of any.
///
/// A thread-local rather than an argument: it is a property of the request,
/// and every write handler and task would otherwise take it only to hand it
/// on to [`crate::state::RepoState::writer`].
pub fn current_context() -> WriteContext {
    CONTEXT.with(|c| c.borrow().clone()).unwrap_or_else(|| WriteContext::new("daemon"))
}

/// Makes `context` this thread's [`current_context`] until the guard drops.
pub fn enter_context(context: WriteContext) -> ContextGuard {
    let previous = CONTEXT.with(|c| c.borrow_mut().replace(context));
    ContextGuard(previous)
}

/// Puts the thread's previous context back.
pub struct ContextGuard(Option<WriteContext>);

impl Drop for ContextGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        CONTEXT.with(|c| *c.borrow_mut() = previous);
    }
}

/// Refuses a label a client may not write under: one that does not exist,
/// or the watcher's or one inside it.
pub fn check_client_label(log: &dyn Log, id: i64) -> Result<()> {
    let watcher = log.watcher_label()?;
    let mut next = Some(id);
    let mut depth = 0;
    while let Some(label) = next {
        if Some(label) == watcher {
            return Err(DomainError::BadRequest(format!(
                "label {id} is the watcher's: only the daemon writes under it"
            ))
            .into());
        }
        let Some(meta) = log.labels(&[label])?.remove(&label) else {
            return Err(DomainError::BadRequest(format!("no label {label}")).into());
        };
        next = meta.parent;
        depth += 1;
        if depth > MAX_TREE_DEPTH {
            return Err(DomainError::BadRequest(format!("label {id} is nested too deep")).into());
        }
    }
    Ok(())
}

// ── Targets ──────────────────────────────────────────────────────────────────

/// An operation named by a request (a prune's cutoff).
#[derive(Debug)]
pub enum Target {
    Id(i64),
    Timestamp(i64),
    /// A label: its oldest operation, its children's included — so a prune
    /// to it keeps it whole.
    Label(i64),
}

/// Resolves a target to an operation id.
pub fn resolve_target(log: &dyn Log, target: &Target) -> Result<i64> {
    match target {
        Target::Id(id) => {
            log.op(*id)?
                .ok_or_else(|| DomainError::NotFound(format!("operation {id} not found")))?;
            Ok(*id)
        }
        Target::Timestamp(t) => log
            .last_op_at_or_before(*t)?
            .with_context(|| format!("no operation found at or before timestamp {t}")),
        Target::Label(label) => first_op_under(log, *label)?.ok_or_else(|| {
            DomainError::NotFound(format!("label {label} holds no operation")).into()
        }),
    }
}

/// The oldest operation of `label` and its descendants.
pub fn first_op_under(log: &dyn Log, label: i64) -> Result<Option<i64>> {
    let mut first: Option<i64> = None;
    let mut todo = vec![label];
    while let Some(label) = todo.pop() {
        if let Some(op) = log.label_first_op(label)? {
            first = Some(first.map_or(op, |f| f.min(op)));
        }
        todo.extend(log.label_children(label)?);
    }
    Ok(first)
}

/// `label` and every label nested in it, `label` first.
pub fn label_subtree(log: &dyn Log, label: i64) -> Result<Vec<i64>> {
    let mut out = vec![label];
    let mut i = 0;
    while i < out.len() {
        out.extend(log.label_children(out[i])?);
        i += 1;
    }
    Ok(out)
}

/// The fields that decide which folders are watched: a write that moves one
/// of them recomputes the watch set.
const DECIDES_WATCHES: &[&str] = &["mf_watch", "mf_ignore", crate::eligibility::WATCH_EXCEEDED];

/// Outcome of [`Writer::retype_field`]: how many rows were converted and the
/// metarecords whose values could not be converted and fell back to the
/// target's sentinel (deduped, sorted).
pub struct RetypeSummary {
    pub converted: usize,
    pub fallback_uuids: Vec<Uuid>,
}

// ── Pruning (doc "Pruning the log") ────────────────────────────────────

/// Permanently removes every operation older than `target` — the target
/// itself is kept. Returns (pruned operations, labels left empty and dropped).
pub fn prune(conn: &mut dyn Database, target: i64) -> Result<(usize, usize)> {
    conn.head()?.context("cannot prune an empty history")?;
    // What goes is read, and nothing else: the operations older than the
    // target, never the ones kept.
    let doomed: Vec<i64> = conn.ops_back(target - 1, None)?.into_iter().map(|o| o.id).collect();
    let tx = conn.begin_write()?;
    let labels = tx.delete_ops(&doomed)?;
    tx.commit()?;
    Ok((doomed.len(), labels))
}

/// Buffered operations are flushed to the database once this many accumulate,
/// keeping the Writer's memory bounded on huge writes (e.g. reconcile).
pub const FLUSH_THRESHOLD: usize = 4096;

/// A `(field name, metarecord)` cell of a forest an operation moved a position
/// of — added, removed or replaced. Taken from the rows the operation wrote, not
/// read back from the store. The watch rule index reads it to tell whether a
/// write moved a metarecord a rule depends on (`RepoState::settle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedCell {
    pub field: String,
    pub uuid: Uuid,
}

/// The cells one operation moved: every `tree_ref` field among the rows it
/// wrote or took away (`after`, `before`), once each, in row order.
fn moved_cells_of(before: &[FieldRow], after: &[FieldRow], entity: Uuid) -> Vec<MovedCell> {
    let mut out: Vec<MovedCell> = Vec::new();
    for row in before.iter().chain(after) {
        if row.value.position().is_some() && !out.iter().any(|c| c.field == row.name) {
            out.push(MovedCell { field: row.name.clone(), uuid: entity });
        }
    }
    out
}

/// What a committed write obliges its caller to bring back in step — the
/// in-memory state a transaction cannot update itself (see
/// `RepoState::settle`). Read off the writer *before* `commit` consumes it.
#[derive(Debug, Default, Clone)]
pub struct WriteEffects {
    /// The cells the write moved a position of, one entry per operation and
    /// field, in write order. Never truncated: a write that changed the
    /// whole forest is exactly the one whose upkeep must not be guessed at.
    tree: Vec<MovedCell>,
    /// HEAD when the write began: what the in-memory state settled by this
    /// write must have described for a cheap upkeep to be enough.
    base_head: Option<i64>,
    /// Whether the write wrote a field that decides which directories are
    /// watched.
    watch: bool,
}

impl WriteEffects {
    /// True if any `tree_ref` field row was created or removed.
    pub fn touches_tree(&self) -> bool {
        !self.tree.is_empty()
    }

    /// The cells the write moved a position of, in write order.
    pub fn moved_cells(&self) -> &[MovedCell] {
        &self.tree
    }

    /// True if the write wrote `mf_watch` / `mf_ignore` (eligibility) or
    /// `mfr_watch_exceeded` (the watch budget). A caller that keeps a live
    /// inotify watch set must refresh it: the set of watched directories may
    /// have changed.
    pub fn touches_watch(&self) -> bool {
        self.watch
    }

    /// HEAD when the write began.
    pub fn base_head(&self) -> Option<i64> {
        self.base_head
    }
}

// ── Automatic retention (doc "Automatic retention") ────────────────

/// How much history the log keeps: a number of operations per origin — the
/// watcher's, and everything else (doc "Automatic retention"). Applied by
/// [`Writer::commit`], inside the write's own transaction: a trim deletes the
/// oldest operations of an origin, which costs less than the append that
/// triggered it and adds no `fsync` of its own.
///
/// Two budgets because the volumes are not comparable: a watched tree writes
/// operations all day, and a limit shared with them would let that churn push
/// the user's own history out. Nothing in the log depends on an older
/// operation being there (doc "The operation log"), so either end can go.
///
/// Deliberately *not* a [`prune`]: no `VACUUM`. In a steady state the freed
/// pages are what the next appends write into, so the database settles on a
/// plateau instead of being rewritten whole at every trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Operations of the watcher to keep. `0` = unlimited.
    pub watcher_ops: u64,
    /// Operations of every other origin to keep. `0` = unlimited.
    pub user_ops: u64,
}

impl Retention {
    /// Keep everything. Deliberately not a `Default` impl: the default a
    /// repository actually writes with is the configured one, and a silent
    /// `Retention::default()` meaning "unlimited" would read as agreeing with
    /// it.
    pub const UNLIMITED: Self = Self { watcher_ops: 0, user_ops: 0 };

    /// How far past a limit the log is allowed to drift before a trim runs.
    /// Hysteresis, not sloppiness: trimming one operation per write would
    /// dirty the oldest (cold) pages of the log at every single commit, where
    /// one trim per `slack` operations costs the same work spread over `slack`
    /// transactions that were going to write anyway.
    pub fn slack(limit: u64) -> u64 {
        (limit / 10).max(1)
    }

    pub(crate) fn enabled(&self) -> bool {
        self.watcher_ops > 0 || self.user_ops > 0
    }
}

/// A single logged write transaction. All changes made through one Writer
/// form one write; commit is atomic. Dropping a Writer without committing
/// rolls everything back. After any method returns an error, the Writer must
/// be dropped (the whole write is abandoned).
///
/// Data-table changes are applied immediately (later changes and lookups in
/// the same write observe them); the log rows are buffered and inserted
/// in bulk, in batches of [`FLUSH_THRESHOLD`] operations.
pub struct Writer<'c> {
    tx: Box<dyn WriteTxn + 'c>,
    /// The label its operations go under.
    label_id: i64,
    /// Whether this write opened that label, which then goes with it when
    /// nothing was written.
    opened_label: bool,
    /// When this write began: every operation it records carries it.
    timestamp: i64,
    /// The newest operation: HEAD as of `begin`, then the last one this
    /// write flushed. What `commit` records as the log's head.
    chain_head: Option<i64>,
    /// Number of operations already flushed to the database.
    flushed: i64,
    /// The id of this write's first operation, once flushed: where the
    /// retention trim stops (see [`Self::commit`]).
    first_op: Option<i64>,
    pending: Vec<NewOp>,
    /// Per-write cache of each field name's established (non-`Nothing`) value
    /// type, populated lazily on the first checked write of a name. Collapses the
    /// per-write type probe to one DB seek per field name (a bulk reconcile/watcher
    /// write writes the same ~8 reserved names across thousands of records).
    field_types: HashMap<String, String>,
    /// Field names whose type check this write deferred to commit, and the
    /// reason it may: see [`Self::defer_type_checks`]. Empty for every ordinary
    /// write, which is checked as it happens.
    deferred_types: Option<HashSet<String>>,
    /// The `tree_ref` positions this write wrote with their check deferred
    /// to commit (see [`Self::defer_type_checks`]): `(metarecord, field,
    /// value)`. `None` for every ordinary write, checked as it happens.
    deferred_trees: Option<std::cell::RefCell<Vec<(Uuid, String, Value)>>>,
    /// How much history to keep behind HEAD; applied on commit.
    retention: Retention,
    /// What this write will oblige its caller to refresh, accumulated as the
    /// operations are recorded rather than read back off `pending` — which the
    /// flush above empties, so a long write used to forget its early writes.
    effects: WriteEffects,
    /// The cells a *manual* operation took a `tree_ref` row from, checked once
    /// at commit (see [`Self::check_forest_integrity`]). Deduplicated through
    /// its own index — a watcher flush writes one `mfr_path` op per file, and
    /// scanning the list each time made a batch quadratic in itself; the list
    /// keeps write order for a stable error.
    tree_lost: Vec<(String, Uuid)>,
    tree_lost_seen: HashMap<String, HashSet<Uuid>>,
    /// The operation the writes recorded from now on are undoing, stamped onto
    /// each of them as `reverts_op_id` (see [`Self::reverting`]). `None` for
    /// every ordinary write.
    reverting: Option<i64>,
    /// Who this write is on behalf of (see [`Self::set_origin`]), stamped
    /// onto each operation it records.
    origin: Option<String>,
}

impl<'c> Writer<'c> {
    /// Opens a transaction under a label of its own named `label` (or
    /// "write"), keeping the whole history. Every write of a *loaded*
    /// repository goes through [`crate::state::RepoState::writer`] instead,
    /// which applies the repository's configured retention.
    pub fn begin(conn: &'c mut dyn Begin, label: Option<String>) -> Result<Self> {
        Self::begin_with_retention(conn, label, Retention::UNLIMITED)
    }

    /// [`Self::begin`], dropping the history that falls outside `retention`
    /// when the write is committed.
    pub fn begin_with_retention(
        conn: &'c mut dyn Begin,
        label: Option<String>,
        retention: Retention,
    ) -> Result<Self> {
        let name = label.unwrap_or_else(|| "write".to_string());
        Self::begin_in(conn, WriteContext::new(name), retention)
    }

    /// Opens a transaction writing in `context` (doc "Labels and contexts").
    /// A label named by a client must exist and lie outside the watcher's
    /// (`400` otherwise): a client cannot pass its writes off as the
    /// daemon's.
    pub fn begin_in(
        conn: &'c mut dyn Begin,
        context: WriteContext,
        retention: Retention,
    ) -> Result<Self> {
        let tx = conn.begin_write()?;
        let head = tx.head()?;
        let timestamp = now_ms();
        let mut origin = None;
        let (label_id, opened_label) = match context {
            WriteContext::In(id) => {
                check_client_label(&*tx, id)?;
                (id, false)
            }
            WriteContext::New(label) => {
                if let Some(parent) = label.parent {
                    check_client_label(&*tx, parent)?;
                }
                let meta = LabelMeta {
                    name: label.name,
                    description: label.description,
                    parent: label.parent,
                    created: timestamp,
                };
                (tx.create_label(&meta)?, true)
            }
            WriteContext::Command { key, label } => match tx.label_of_command(key)? {
                Some(id) => (id, false),
                None => {
                    if let Some(parent) = label.parent {
                        check_client_label(&*tx, parent)?;
                    }
                    let meta = LabelMeta {
                        name: label.name,
                        description: label.description,
                        parent: label.parent,
                        created: timestamp,
                    };
                    (tx.create_command_label(&meta, key)?, true)
                }
            },
            WriteContext::Watcher => {
                origin = Some(ORIGIN_WATCHER.to_string());
                let root = match tx.watcher_label()? {
                    Some(root) if !tx.labels(&[root])?.is_empty() => root,
                    _ => {
                        let root = tx.create_label(&LabelMeta {
                            name: "watcher".to_string(),
                            description: Some(
                                "What the daemon recorded for the filesystem".to_string(),
                            ),
                            parent: None,
                            created: timestamp,
                        })?;
                        tx.set_watcher_label(root)?;
                        root
                    }
                };
                let meta = LabelMeta {
                    name: "watcher flush".to_string(),
                    description: None,
                    parent: Some(root),
                    created: timestamp,
                };
                (tx.create_label(&meta)?, true)
            }
        };
        Ok(Self {
            tx,
            label_id,
            opened_label,
            timestamp,
            chain_head: head,
            flushed: 0,
            first_op: None,
            pending: Vec::new(),
            field_types: HashMap::new(),
            deferred_types: None,
            deferred_trees: None,
            retention,
            effects: WriteEffects { base_head: head, ..WriteEffects::default() },
            tree_lost: Vec::new(),
            tree_lost_seen: HashMap::new(),
            reverting: None,
            origin,
        })
    }

    /// The label this write's operations go under.
    pub fn label_id(&self) -> i64 {
        self.label_id
    }

    /// Notes that this write reverts label `target`: all of it, its children
    /// included, when `tree`, some of its operations otherwise — what the log's
    /// summaries say of an undo without reading its operations.
    pub fn note_label_reverts(&mut self, target: i64, tree: bool) -> Result<()> {
        self.tx.note_label_reverts(self.label_id, target, tree)
    }

    /// Marks this write as made on someone's behalf rather than at a
    /// client's plain request (doc "Operations and their origin"): a trashing.
    /// The watcher's flush gets [`ORIGIN_WATCHER`] from its context
    /// ([`WriteContext::Watcher`]).
    ///
    /// It is the operation's origin, not its type, that says this: a file
    /// arriving is recorded as a `create_metarecord`, indistinguishable by
    /// type from a metarecord the user created.
    pub fn set_origin(&mut self, origin: &str) -> Result<()> {
        self.origin = Some(origin.to_string());
        Ok(())
    }

    /// Read access to the transaction, for lookups that must observe the
    /// writes already applied.
    pub fn store(&self) -> &dyn Store {
        &*self.tx
    }

    /// The dense ids of `uuids` that name a metarecord.
    pub fn dense_ids(&self, uuids: &[Uuid]) -> Result<RoaringBitmap> {
        Ok(self.tx.dense_ids(uuids, false)?.unwrap_or_default())
    }

    /// The dense ids of `uuids`, every one of which must name a metarecord
    /// (`NotFound` otherwise, as a write to it alone would say).
    pub fn dense_ids_existing(&self, uuids: &[Uuid]) -> Result<RoaringBitmap> {
        Ok(self.tx.dense_ids(uuids, true)?.unwrap_or_default())
    }

    /// Those of `ids` that hold a row of `field`, the transaction's writes so
    /// far counted; `None` when the store cannot say without reading them.
    pub fn holders_among(&self, field: &str, ids: &RoaringBitmap) -> Result<Option<RoaringBitmap>> {
        Ok(self.tx.holder_ids(field)?.map(|holders| &holders & ids))
    }

    /// The uuids of dense ids, in the order of the ids.
    pub fn uuids_of(&self, ids: &RoaringBitmap) -> Result<Vec<Uuid>> {
        self.tx.uuids_of(ids)
    }

    /// Number of operations recorded so far in this write.
    pub fn op_count(&self) -> i64 {
        self.flushed + self.pending.len() as i64
    }

    /// What this write obliges its caller to bring back in step (watch
    /// rules, watch set). Read it before [`Self::commit`], which consumes the writer.
    pub fn effects(&self) -> WriteEffects {
        self.effects.clone()
    }

    /// Records what one operation implies: what its caller will have to bring
    /// back in step ([`WriteEffects`]), and which cells lost a `tree_ref` row
    /// and must therefore be checked at commit.
    fn observe_effects(
        &mut self,
        op_type: OpType,
        before: &[FieldRow],
        after: &[FieldRow],
        entity: Uuid,
    ) {
        for row in before {
            if op_type.is_manual()
                && row.value.position().is_some()
                && !self
                    .tree_lost_seen
                    .get(row.name.as_str())
                    .is_some_and(|seen| seen.contains(&entity))
            {
                self.tree_lost.push((row.name.clone(), entity));
                self.tree_lost_seen.entry(row.name.clone()).or_default().insert(entity);
            }
        }
        for row in before.iter().chain(after) {
            if DECIDES_WATCHES.contains(&row.name.as_str()) {
                self.effects.watch = true;
            }
        }
        self.effects.tree.extend(moved_cells_of(before, after, entity));
    }

    /// The other half of [`Self::validate_tree_ref`]: a `tree_ref` reference is
    /// refused when it *names* a parent carrying no position of its own, so a
    /// position may not be *removed* while metarecords are placed under it.
    ///
    /// Checked once, on the state the write is about to commit, rather than
    /// per operation — deleting a whole subtree is legitimate even though the
    /// parent goes first, and only the committed state has to be a forest.
    ///
    /// Without it the children stayed, naming a node that no longer existed:
    /// *detached*, reachable by uuid but under no parent and in no roots map, so
    /// `GET /tree/roots` and path reconstruction disagreed about them ever after.
    /// Defers this write's type checks — and its forest checks — to
    /// [`Self::commit`], where they are made against the state the write
    /// lands on rather than against each intermediate one.
    ///
    /// Only a revert needs this, and only because of how it works: it undoes a
    /// set of operations one inverse at a time, newest to oldest, and the states
    /// in between need not be type-consistent even when the final one is.
    /// Undoing a `retype_field` is the case — after the last converted row's
    /// `append_field` is undone, the cell still holds the others in the *new*
    /// type.
    ///
    /// Deferring is not skipping: [`Self::check_deferred_types`] refuses a
    /// write that leaves two value types under one field name, which is what
    /// the per-write check was protecting.
    pub fn defer_type_checks(&mut self) {
        self.deferred_types.get_or_insert_with(Default::default);
        // The same holds for the forest: a revert recreates a folder's file
        // before the folder when the folder was deleted first (a trashing's
        // cascade), so a position may name a parent not there *yet*.
        self.deferred_trees.get_or_insert_with(Default::default);
        // The per-write cache holds types probed under the eager rule; drop
        // it so nothing downstream reads a type this write is about to move.
        self.field_types.clear();
    }

    /// Declares that the operations recorded from now on undo `op_id`, which
    /// each of them records as its `reverts_op_id` (doc "Revert").
    /// A revert sets it around each operation it walks; `None` restores the
    /// ordinary, unattributed write.
    pub fn reverting(&mut self, op_id: Option<i64>) {
        self.reverting = op_id;
    }

    /// The deferred check: every field name this write wrote must carry a
    /// single value type across the repository (doc "One value type per field name").
    fn check_deferred_types(&self) -> Result<()> {
        let Some(names) = &self.deferred_types else { return Ok(()) };
        for name in names {
            let types = self.tx.value_types(name)?;
            if types.len() > 1 {
                return Err(DomainError::BadRequest(format!(
                    "field '{name}' would be left with more than one value type ({}); \
                     the value types recorded under one field name must agree",
                    types.join(", ")
                ))
                .into());
            }
        }
        Ok(())
    }

    /// The deferred forest check: every position the write wrote that is
    /// still there must name a parent that is, without a cycle or too deep a
    /// chain — checked once, on the state the write lands on.
    fn check_deferred_trees(&self) -> Result<()> {
        let Some(deferred) = &self.deferred_trees else { return Ok(()) };
        for (uuid, name, value) in deferred.borrow().iter() {
            if self.tx.rows_named(*uuid, name)?.iter().any(|r| &r.value == value) {
                self.check_tree_ref(*uuid, name, value)?;
            }
        }
        Ok(())
    }

    fn check_forest_integrity(&self) -> Result<()> {
        for (field_name, uuid) in &self.tree_lost {
            // Cheapest question first, and the one that is almost always "none":
            // a cell nobody is placed under has nothing to keep.
            let children = self.tx.children(field_name, *uuid)?;
            if children.is_empty() {
                continue;
            }
            let placed = self.tx.rows_named(*uuid, field_name)?;
            if placed.iter().any(|r| r.value.position().is_some()) {
                continue;
            }
            let mut named: Vec<String> =
                children.iter().take(3).map(|(_, name)| format!("{name:?}")).collect();
            if children.len() > named.len() {
                named.push(format!("and {} more", children.len() - named.len()));
            }
            return Err(DomainError::BadRequest(format!(
                "cannot remove the last '{field_name}' position of {uuid}: {} metarecord(s) \
                 are placed under it ({}); move or delete them first",
                children.len(),
                named.join(", ")
            ))
            .into());
        }
        Ok(())
    }

    /// Removes every row of `(uuid, name)`, leaving the field unknown.
    /// Set-field shaped (before = all rows, after = none) so the standard
    /// `set_field` inverse applies. Used to invalidate `mfr_*` hashes.
    pub fn clear_field_as(&mut self, op_type: OpType, uuid: Uuid, name: &str) -> Result<()> {
        let before = self.tx.rows_named(uuid, name)?;
        if before.is_empty() {
            return Ok(());
        }
        self.must_exist(uuid)?;
        self.tx.delete_rows(uuid, Some(name))?;
        self.log_op(op_type, uuid, Some(name), before, vec![])?;
        self.field_types.remove(name); // rows removed: the type may have unlocked
        Ok(())
    }

    /// Creates a new metarecord owned by this repository.
    pub fn create_metarecord(&mut self, fields: Vec<Field>) -> Result<MetaRecord> {
        self.create_metarecord_with_uuid(Uuid::new_v4(), fields)
    }

    /// Like [`create_metarecord`] but at a caller-supplied UUID (sync
    /// bare-record creation, doc "Sync endpoints"). The UUID must not already exist — the
    /// `metarecord` PRIMARY KEY rejects a duplicate.
    pub fn create_metarecord_with_uuid(
        &mut self,
        uuid: Uuid,
        fields: Vec<Field>,
    ) -> Result<MetaRecord> {
        // Repeated (name, value) pairs are written once (doc "No duplicate rows");
        // the record returned mirrors what is stored.
        let fields = collapse_duplicate_fields(fields);
        validate_one_position_each(uuid, fields.iter().map(|f| (f.name.as_str(), &f.value)))?;
        for f in &fields {
            self.validate_tree_ref(uuid, &f.name, &f.value)?;
        }
        // Seeded with the metarecord's own term; `log_op` then adds the terms
        // of the rows created below, so a fresh record's version describes its
        // initial fields like any other (doc "Metarecord version").
        self.tx.create_metarecord(uuid)?;

        let mut after = Vec::with_capacity(fields.len());
        let mut out_fields = Vec::with_capacity(fields.len());
        for f in fields {
            // Checked inside the loop so two rows of the same name with different
            // types within one create are rejected (the second sees the first).
            self.validate_value_type(&f.name, &f.value)?;
            self.tx.insert_row(uuid, &f.name, &f.value)?;
            after.push(FieldRow { name: f.name.clone(), value: f.value.clone() });
            out_fields.push(f);
        }

        self.log_op(OpType::CreateRecord, uuid, None, vec![], after)?;
        let version = self.current_version(uuid)?;
        Ok(MetaRecord { uuid, version, fields: out_fields })
    }

    /// Deletes a metarecord and all its rows.
    ///
    /// One metarecord is never deleted: the repository root, the `mfr_path` root
    /// position (doc "TreeRef path conventions"). Every path
    /// hangs from it, and re-creating it is not a repair — a fresh metarecord
    /// gets a fresh uuid, while the whole forest keeps naming the deleted one.
    /// Recovery for a root deleted before this check existed is a
    /// revert of the deletion, which restore it at its own uuid.
    pub fn delete_metarecord(&mut self, uuid: Uuid) -> Result<()> {
        self.must_exist(uuid)?;
        let before = self.tx.rows(uuid)?;
        let is_repository_root = before.iter().any(|row| {
            row.name == "mfr_path"
                && matches!(&row.value, Value::TreeRef { parent: None, name } if name.as_bytes().is_empty())
        });
        if is_repository_root {
            return Err(DomainError::BadRequest(
                "cannot delete the repository root metarecord".to_string(),
            )
            .into());
        }
        self.tx.remove_metarecord(uuid)?;
        self.log_op(OpType::DeleteRecord, uuid, None, before, vec![])?;
        Ok(())
    }

    /// Replaces the *entire* field set of an existing metarecord, keeping its
    /// UUID, in one `SetRecord` operation (before = all old fields, after = the
    /// new set) — the whole-record analogue of create/delete. Literal overwrite:
    /// every old row is dropped, including reserved ones not in `fields`.
    pub fn set_record(&mut self, uuid: Uuid, fields: Vec<Field>) -> Result<MetaRecord> {
        let fields = collapse_duplicate_fields(fields); // doc "No duplicate rows"
        validate_one_position_each(uuid, fields.iter().map(|f| (f.name.as_str(), &f.value)))?;
        self.must_exist(uuid)?;
        let before = self.tx.rows(uuid)?;
        self.tx.delete_rows(uuid, None)?;
        // The whole record's types are reset; drop cached locks so the new set
        // re-probes from a clean slate (validated against the post-delete state).
        for row in &before {
            self.field_types.remove(&row.name);
        }
        let mut after = Vec::with_capacity(fields.len());
        let mut out_fields = Vec::with_capacity(fields.len());
        for f in fields {
            self.validate_tree_ref(uuid, &f.name, &f.value)?;
            self.validate_value_type(&f.name, &f.value)?;
            self.tx.insert_row(uuid, &f.name, &f.value)?;
            after.push(FieldRow { name: f.name.clone(), value: f.value.clone() });
            out_fields.push(f);
        }
        self.log_op(OpType::SetRecord, uuid, None, before, after)?;
        // A hash of the content (doc "Metarecord version"): read back, never
        // derived from the old one.
        let version = self.current_version(uuid)?;
        Ok(MetaRecord { uuid, version, fields: out_fields })
    }

    /// Replaces all rows for `(uuid, name)` with a single value.
    pub fn set_field(&mut self, uuid: Uuid, name: &str, value: Value) -> Result<()> {
        self.set_field_as(OpType::SetField, uuid, name, value)
    }

    /// `set_field` recorded under a watcher-specific op type
    /// (`file_deleted`, `file_moved`, `file_modified`).
    pub fn set_field_as(
        &mut self,
        op_type: OpType,
        uuid: Uuid,
        name: &str,
        value: Value,
    ) -> Result<()> {
        self.validate_tree_ref(uuid, name, &value)?;
        self.validate_value_type(name, &value)?;
        self.must_exist(uuid)?;
        let before = self.tx.rows_named(uuid, name)?;
        self.tx.delete_rows(uuid, Some(name))?;
        let cleared_to_nothing = matches!(value, Value::Nothing);
        self.tx.insert_row(uuid, name, &value)?;
        let after = vec![FieldRow { name: name.to_string(), value }];
        self.log_op(op_type, uuid, Some(name), before, after)?;
        if cleared_to_nothing {
            // The only remaining row is Nothing: the type may have unlocked.
            self.field_types.remove(name);
        }
        Ok(())
    }

    /// Replaces all rows for `(uuid, name)` with the given set of values
    /// (multi-map), in a single `SetField` operation. A revert's `set_field`
    /// inverse restores *all* snapshot rows, so this needs nothing of its own.
    pub fn set_field_multi(&mut self, uuid: Uuid, name: &str, values: Vec<Value>) -> Result<()> {
        self.set_field_multi_as(OpType::SetField, uuid, name, values)
    }

    /// [`Self::set_field_multi`] under a chosen op type: a revert writes the
    /// watcher types when it undoes a file event (doc "What a revert writes").
    /// A repeated value is collapsed to a single row (doc "No duplicate rows").
    pub fn set_field_multi_as(
        &mut self,
        op_type: OpType,
        uuid: Uuid,
        name: &str,
        values: Vec<Value>,
    ) -> Result<()> {
        validate_one_position_each(uuid, values.iter().map(|v| (name, v)))?;
        for value in &values {
            self.validate_tree_ref(uuid, name, value)?;
            self.validate_value_type(name, value)?;
        }
        let kept = collapse_duplicates(&values);
        self.must_exist(uuid)?;
        let before = self.tx.rows_named(uuid, name)?;
        self.tx.delete_rows(uuid, Some(name))?;
        let cleared_to_nothing = values.iter().all(|v| matches!(v, Value::Nothing));
        let mut after = Vec::with_capacity(kept.len());
        for value in kept {
            self.tx.insert_row(uuid, name, &value)?;
            after.push(FieldRow { name: name.to_string(), value });
        }
        self.log_op(op_type, uuid, Some(name), before, after)?;
        if cleared_to_nothing {
            // No non-Nothing row remains: the type may have unlocked.
            self.field_types.remove(name);
        }
        Ok(())
    }

    /// Appends one row without touching existing rows of that name.
    /// A value the metarecord already holds under that name is *not* appended
    /// again (doc "No duplicate rows"): nothing is written, nothing
    /// is logged, the version does not move, and the answer is
    /// [`Appended::AlreadyPresent`].
    pub fn append_field(&mut self, uuid: Uuid, name: &str, value: Value) -> Result<Appended> {
        self.validate_one_position(uuid, name, &value, None)?;
        self.validate_tree_ref(uuid, name, &value)?;
        self.validate_value_type(name, &value)?;
        if self.tx.rows_named(uuid, name)?.iter().any(|r| r.value == value) {
            return Ok(Appended::AlreadyPresent);
        }
        self.must_exist(uuid)?;
        self.tx.insert_row(uuid, name, &value)?;
        let after = vec![FieldRow { name: name.to_string(), value }];
        self.log_op(OpType::AppendField, uuid, Some(name), vec![], after)?;
        Ok(Appended::Created)
    }

    /// The logged delete+append core of [`Self::replace_value`], without the
    /// invariant checks. Shared with [`Self::retype_field`], which is itself the
    /// authority that changes a field's established type (so it must not be
    /// rejected by the per-write type check while converting row by row).
    /// `new_name` may differ from `old.name` (a rename).
    fn replace_owned_row(
        &mut self,
        uuid: Uuid,
        old: FieldRow,
        new_name: &str,
        value: Value,
    ) -> Result<()> {
        self.tx.delete_row(uuid, &old.name, &old.value)?;
        self.log_op(OpType::DeleteField, uuid, Some(&old.name.clone()), vec![old.clone()], vec![])?;

        self.tx.insert_row(uuid, new_name, &value)?;
        let after = vec![FieldRow { name: new_name.to_string(), value }];
        self.log_op(OpType::AppendField, uuid, Some(new_name), vec![], after)?;
        if new_name != old.name {
            // A rename can unlock the old name's type and lock the new one.
            self.field_types.remove(&old.name);
            self.field_types.remove(new_name);
        }
        Ok(())
    }

    /// Converts every non-`Nothing` row of field `name` to the type `to`,
    /// repository-wide, in this one write (doc "Changing a field's type"). The target may be any
    /// type, including the reference variants.
    /// Row-scoped (each row keeps its id) and
    /// bypasses the per-write type check (it *is* the type change). `Nothing`
    /// rows are left untouched (explicit absence is preserved). A `String →
    /// TreeRef` result that would violate the forest (missing parent, cycle,
    /// depth) is demoted to `Nothing` rather than aborting the whole retype, so
    /// one bad path never blocks the rest. Returns how many rows changed and the
    /// metarecords whose values fell back to the sentinel.
    pub fn retype_field(&mut self, name: &str, to: FieldType) -> Result<RetypeSummary> {
        // The field's established type changes here; drop any cached entry so a
        // later checked write in this write re-probes.
        self.field_types.remove(name);
        let mut converted = 0usize;
        let mut fallback: std::collections::BTreeSet<Uuid> = Default::default();
        for uuid in self.tx.holders(name)? {
            for row in self.tx.rows_named(uuid, name)? {
                if matches!(row.value, Value::Nothing) {
                    continue;
                }
                let (mut new_value, mut fell_back) = row.value.convert_to(to);
                // A converted TreeRef must satisfy the forest invariants like any
                // other write; a violating value is demoted to the Nothing
                // sentinel (and reported) so the retype as a whole still succeeds.
                if new_value.position().is_some() {
                    let valid = self
                        .validate_one_position(uuid, &row.name, &new_value, Some(&row.value))
                        .and_then(|()| self.validate_tree_ref(uuid, &row.name, &new_value));
                    if let Err(e) = valid {
                        if e.downcast_ref::<DomainError>().is_some() {
                            new_value = Value::Nothing;
                            fell_back = true;
                        } else {
                            return Err(e); // a genuine DB error, not a forest violation
                        }
                    }
                }
                if new_value == row.value {
                    continue; // already the target type
                }
                if fell_back {
                    fallback.insert(uuid);
                }
                let name = row.name.clone();
                // Two values the conversion made equal are one row afterwards,
                // not a duplicate (doc "No duplicate rows"). The
                // probe reads the transaction's own state, so rows converted
                // earlier in this pass count as siblings.
                let twin = self.tx.rows_named(uuid, &name)?.iter().any(|r| r.value == new_value);
                if twin {
                    self.delete_field_rows(uuid, &name, vec![row])?;
                } else {
                    self.replace_owned_row(uuid, row, &name, new_value)?;
                }
                converted += 1;
            }
        }
        Ok(RetypeSummary { converted, fallback_uuids: fallback.into_iter().collect() })
    }

    /// Deletes the given rows of `(uuid, name)` and logs them as *one*
    /// `DeleteField` operation (before = the rows, after = empty), so a bulk
    /// removal is one reversible log line per metarecord rather than one per row.
    /// Returns the number removed; logs nothing when `rows` is empty.
    fn delete_field_rows(&mut self, uuid: Uuid, name: &str, rows: Vec<FieldRow>) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.must_exist(uuid)?;
        for row in &rows {
            self.tx.delete_row(uuid, &row.name, &row.value)?;
        }
        let removed = rows.len();
        self.log_op(OpType::DeleteField, uuid, Some(name), rows, vec![])?;
        self.field_types.remove(name); // rows removed: the type may have unlocked
        Ok(removed)
    }

    /// Removes every row of `(uuid, name)` whose value equals `value` — the
    /// inverse of [`Self::append_field`] — in one operation. Returns the count.
    pub fn delete_fields_valued(&mut self, uuid: Uuid, name: &str, value: &Value) -> Result<usize> {
        let rows: Vec<FieldRow> =
            self.tx.rows_named(uuid, name)?.into_iter().filter(|r| &r.value == value).collect();
        self.delete_field_rows(uuid, name, rows)
    }

    /// Removes the field *entirely* — every row of `(uuid, name)`, whatever its
    /// value — in one operation, leaving the field unknown (absent).
    pub fn delete_fields_named(&mut self, uuid: Uuid, name: &str) -> Result<usize> {
        let rows = self.tx.rows_named(uuid, name)?;
        self.delete_field_rows(uuid, name, rows)
    }

    /// Removes `value` from `(uuid, name)`, the other values staying; not
    /// found when the field does not hold it.
    pub fn remove_value(&mut self, uuid: Uuid, name: &str, value: &Value) -> Result<()> {
        if self.delete_fields_valued(uuid, name, value)? == 0 {
            return Err(no_such_value(uuid, name));
        }
        Ok(())
    }

    /// Replaces the value `from` of `(uuid, name)` by `to`, the other values
    /// staying; not found when the field does not hold `from`. Onto a value
    /// the field already holds, the two merge: `from` goes, and the state is
    /// the one asked for (doc "No duplicate rows").
    pub fn replace_value(&mut self, uuid: Uuid, name: &str, from: &Value, to: Value) -> Result<()> {
        let rows = self.tx.rows_named(uuid, name)?;
        let Some(old) = rows.iter().find(|r| &r.value == from).cloned() else {
            return Err(no_such_value(uuid, name));
        };
        if from == &to {
            return Ok(());
        }
        if rows.iter().any(|r| r.value == to) {
            return self.delete_field_rows(uuid, name, vec![old]).map(|_| ());
        }
        self.validate_one_position(uuid, name, &to, Some(from))?;
        self.validate_tree_ref(uuid, name, &to)?;
        self.validate_value_type(name, &to)?;
        self.replace_owned_row(uuid, old, name, to)
    }

    /// Renames the field `name` of `uuid` to `to`: every value moves. Not
    /// found when the metarecord has no such field; refused when it already
    /// holds `to` — a name the caller most likely forgot, which a merge would
    /// hide.
    pub fn rename_record_field(&mut self, uuid: Uuid, name: &str, to: &str) -> Result<()> {
        let rows = self.tx.rows_named(uuid, name)?;
        if rows.is_empty() {
            return Err(
                DomainError::NotFound(format!("no field '{name}' on metarecord {uuid}")).into()
            );
        }
        if name == to {
            return Ok(());
        }
        if !self.tx.rows_named(uuid, to)?.is_empty() {
            return Err(DomainError::BadRequest(format!(
                "metarecord {uuid} already has a field '{to}'"
            ))
            .into());
        }
        for row in rows {
            // The value type is validated against the *target* name: a rename
            // moves the row into that name's type group.
            self.validate_one_position(uuid, to, &row.value, None)?;
            self.validate_tree_ref(uuid, to, &row.value)?;
            self.validate_value_type(to, &row.value)?;
            let value = row.value.clone();
            self.replace_owned_row(uuid, row, to, value)?;
        }
        Ok(())
    }

    /// Adds `value` to `name` on every one of `uuids` that lacks it, as one
    /// write to many records (doc "Mass writes in the store"); the records it
    /// changed.
    pub fn mass_add(&mut self, name: &str, value: Value, uuids: &[Uuid]) -> Result<Vec<Uuid>> {
        self.validate_value_type(name, &value)?;
        if let Some(ids) = self.tx.dense_ids(uuids, true)? {
            if let Some(changed) =
                self.mass_logged(MassKind::Add, name, std::slice::from_ref(&value), &ids)?
            {
                return self.in_request_order(uuids, &changed);
            }
        }
        let one = value.clone();
        self.mass(MassKind::Add, name, vec![value], uuids, move |w, uuid| {
            Ok(w.append_field(uuid, name, one.clone())?.created())
        })
    }

    /// Removes `value` from `name` on every one of `uuids` that holds it; the
    /// records it changed.
    pub fn mass_remove(&mut self, name: &str, value: &Value, uuids: &[Uuid]) -> Result<Vec<Uuid>> {
        if let Some(ids) = self.tx.dense_ids(uuids, false)? {
            if let Some(changed) =
                self.mass_logged(MassKind::Remove, name, std::slice::from_ref(value), &ids)?
            {
                self.field_types.remove(name); // rows removed: the type may have unlocked
                return self.in_request_order(uuids, &changed);
            }
        }
        self.mass(MassKind::Remove, name, vec![value.clone()], uuids, |w, uuid| {
            Ok(w.delete_fields_valued(uuid, name, value)? > 0)
        })
    }

    /// Replaces the rows of `name` by `values` on every one of `uuids`, which
    /// all count as changed.
    pub fn mass_set(
        &mut self,
        name: &str,
        values: Vec<Value>,
        uuids: &[Uuid],
    ) -> Result<Vec<Uuid>> {
        let kept = collapse_duplicates(&values);
        if !kept.is_empty() {
            for value in &kept {
                self.validate_value_type(name, value)?;
            }
            if let Some(ids) = self.tx.dense_ids(uuids, true)? {
                if let Some(changed) = self.mass_logged(MassKind::Replace, name, &kept, &ids)? {
                    self.field_types.remove(name); // rows replaced: the type may have unlocked
                    return self.in_request_order(uuids, &changed);
                }
            }
        }
        self.mass(MassKind::Replace, name, kept, uuids, move |w, uuid| {
            w.set_field_multi(uuid, name, values.clone())?;
            Ok(true)
        })
    }

    /// Removes `name` from every one of `uuids`; the records that held it.
    pub fn mass_unset(&mut self, name: &str, uuids: &[Uuid]) -> Result<Vec<Uuid>> {
        if let Some(ids) = self.tx.dense_ids(uuids, false)? {
            if let Some(changed) = self.mass_logged(MassKind::Replace, name, &[], &ids)? {
                self.field_types.remove(name); // rows removed: the type may have unlocked
                return self.in_request_order(uuids, &changed);
            }
        }
        self.mass(MassKind::Replace, name, Vec::new(), uuids, |w, uuid| {
            Ok(w.delete_fields_named(uuid, name)? > 0)
        })
    }

    /// Adds `value` to `name` on the records `ids` (live ones) that lack it:
    /// the same write as [`Self::mass_add`], for a caller that holds dense ids
    /// — the answer of a query — and so reads nothing per record. The ids of
    /// the records it changed.
    pub fn mass_add_ids(
        &mut self,
        name: &str,
        value: Value,
        ids: &RoaringBitmap,
    ) -> Result<RoaringBitmap> {
        self.validate_value_type(name, &value)?;
        if let Some(changed) =
            self.mass_logged(MassKind::Add, name, std::slice::from_ref(&value), ids)?
        {
            return Ok(changed);
        }
        let uuids = self.tx.uuids_of(ids)?;
        let changed = self.mass_add(name, value, &uuids)?;
        Ok(self.tx.dense_ids(&changed, false)?.unwrap_or_default())
    }

    /// Removes `value` from `name` on the records `ids` that hold it:
    /// [`Self::mass_add_ids`]'s counterpart of [`Self::mass_remove`].
    pub fn mass_remove_ids(
        &mut self,
        name: &str,
        value: &Value,
        ids: &RoaringBitmap,
    ) -> Result<RoaringBitmap> {
        if let Some(changed) =
            self.mass_logged(MassKind::Remove, name, std::slice::from_ref(value), ids)?
        {
            self.field_types.remove(name); // rows removed: the type may have unlocked
            return Ok(changed);
        }
        let uuids = self.tx.uuids_of(ids)?;
        let changed = self.mass_remove(name, value, &uuids)?;
        Ok(self.tx.dense_ids(&changed, false)?.unwrap_or_default())
    }

    /// Replaces the rows of `name` by `values` on the records `ids`, which all
    /// count as changed: [`Self::mass_set`] for a caller that holds dense ids.
    pub fn mass_set_ids(
        &mut self,
        name: &str,
        values: Vec<Value>,
        ids: &RoaringBitmap,
    ) -> Result<RoaringBitmap> {
        let kept = collapse_duplicates(&values);
        if !kept.is_empty() {
            for value in &kept {
                self.validate_value_type(name, value)?;
            }
            if let Some(changed) = self.mass_logged(MassKind::Replace, name, &kept, ids)? {
                self.field_types.remove(name); // rows replaced: the type may have unlocked
                return Ok(changed);
            }
        }
        let uuids = self.tx.uuids_of(ids)?;
        let changed = self.mass_set(name, values, &uuids)?;
        Ok(self.tx.dense_ids(&changed, false)?.unwrap_or_default())
    }

    /// Takes `name` off the records `ids`: [`Self::mass_unset`] for a caller
    /// that holds dense ids.
    pub fn mass_unset_ids(&mut self, name: &str, ids: &RoaringBitmap) -> Result<RoaringBitmap> {
        if let Some(changed) = self.mass_logged(MassKind::Replace, name, &[], ids)? {
            self.field_types.remove(name); // rows removed: the type may have unlocked
            return Ok(changed);
        }
        let uuids = self.tx.uuids_of(ids)?;
        let changed = self.mass_unset(name, &uuids)?;
        Ok(self.tx.dense_ids(&changed, false)?.unwrap_or_default())
    }

    /// Deletes the metarecords `ids` (those that exist): one entry in the log
    /// and no record read or written when the store can (doc "Mass operations
    /// in the log"), else one by one, as [`Self::delete_metarecord`] does. The
    /// ids of those it deleted.
    pub fn mass_delete_ids(&mut self, ids: &RoaringBitmap) -> Result<RoaringBitmap> {
        // A revert writes what it undoes one operation at a time.
        if self.reverting.is_none() {
            // The ids of this write's operations come first.
            self.flush_pending()?;
            let origin = self.origin.clone();
            if let Some(done) =
                self.tx.mass_delete(ids, self.label_id, self.timestamp, origin.as_deref())?
            {
                if let Some(n) = i64::try_from(done.changed.len()).ok().filter(|&n| n > 0) {
                    self.first_op.get_or_insert(done.last_op - n + 1);
                    self.flushed += n;
                    self.chain_head = Some(done.last_op);
                    // Rows are gone: a field's type may have unlocked.
                    self.field_types.clear();
                }
                if done.touches_watch {
                    self.effects.watch = true;
                }
                return Ok(done.changed);
            }
        }
        let mut deleted = RoaringBitmap::new();
        let uuids = self.tx.uuids_of(ids)?;
        for (id, uuid) in ids.iter().zip(uuids) {
            // An id whose metarecord is gone, or was created again under a new
            // one, names nothing to delete.
            if self.tx.dense_ids(&[uuid], false)?.unwrap_or_default().contains(id) {
                self.delete_metarecord(uuid)?;
                deleted.insert(id);
            }
        }
        Ok(deleted)
    }

    /// Reverts the operations `ops` — consecutive `append_field` or
    /// `delete_field` operations of one mass add or remove, oldest first —
    /// as one entry of the log, with no record read or written. `false`,
    /// nothing written, when the store cannot (the caller then reverts them
    /// one by one).
    pub fn revert_mass_run(&mut self, ops: &[OpRow]) -> Result<bool> {
        let (Some(first), Some(last)) = (ops.first(), ops.last()) else { return Ok(false) };
        let Some(name) = first.field_name.clone() else { return Ok(false) };
        // Putting a value back is a write like any other: its type is checked.
        if first.op_type == "delete_field" {
            for row in self.tx.snapshots(first.id, false)? {
                self.validate_value_type(&name, &row.value)?;
            }
        }
        // The ids of this write's operations come first.
        self.flush_pending()?;
        let origin = self.origin.clone();
        let Some(done) = self.tx.mass_revert(
            first.id,
            last.id,
            self.label_id,
            self.timestamp,
            origin.as_deref(),
        )?
        else {
            return Ok(false);
        };
        if let Some(n) = i64::try_from(done.changed.len()).ok().filter(|&n| n > 0) {
            self.first_op.get_or_insert(done.last_op - n + 1);
            self.flushed += n;
            self.chain_head = Some(done.last_op);
            if DECIDES_WATCHES.contains(&name.as_str()) {
                self.effects.watch = true;
            }
        }
        self.field_types.remove(&name);
        Ok(true)
    }

    /// Gives back the records a deletion still pending took, which are the
    /// subjects of `ops` (consecutive `delete_metarecord` operations of one
    /// mass deletion, oldest first): their ids return to the universe, and
    /// one `create_metarecord` operation per record, reverting the one it
    /// takes back, is logged — no record is written. `false`, nothing
    /// written, when the store cannot (the caller then undoes one by one).
    pub fn undelete_pending(&mut self, ops: &[OpRow]) -> Result<bool> {
        let (Some(first), Some(last)) = (ops.first(), ops.last()) else { return Ok(false) };
        let Some(done) = self.tx.mass_undelete(first.id, last.id)? else { return Ok(false) };
        if done.touches_watch {
            self.effects.watch = true;
        }
        // Rows are back: a field's type may be locked again.
        self.field_types.clear();
        for op in ops.iter().rev() {
            self.reverting = Some(op.id);
            self.log_op(OpType::CreateRecord, op.entity_uuid, None, vec![], vec![])?;
        }
        self.reverting = None;
        Ok(true)
    }

    /// [`Self::mass_delete_ids`] for a caller that holds uuids. The uuids it
    /// deleted, in the order asked.
    pub fn mass_delete(&mut self, uuids: &[Uuid]) -> Result<Vec<Uuid>> {
        let ids = self.tx.dense_ids(uuids, false)?.unwrap_or_default();
        let done = self.mass_delete_ids(&ids)?;
        self.in_request_order(uuids, &done)
    }

    /// The records of `asked`, in the order asked, that `changed` holds.
    fn in_request_order(&self, asked: &[Uuid], changed: &RoaringBitmap) -> Result<Vec<Uuid>> {
        let done: HashSet<Uuid> = self.tx.uuids_of(changed)?.into_iter().collect();
        let mut seen = HashSet::new();
        Ok(asked.iter().copied().filter(|u| done.contains(u) && seen.insert(*u)).collect())
    }

    /// An add or a remove the store does in bulk (doc "Mass operations in the
    /// log"): one entry in the log, no record read or written; the records
    /// that changed. `None` when the store does not do it so, and nothing was
    /// written.
    fn mass_logged(
        &mut self,
        kind: MassKind,
        name: &str,
        values: &[Value],
        ids: &RoaringBitmap,
    ) -> Result<Option<RoaringBitmap>> {
        // A revert writes what it undoes one operation at a time.
        if self.reverting.is_some() {
            return Ok(None);
        }
        // The ids of this write's operations come first.
        self.flush_pending()?;
        let origin = self.origin.clone();
        let Some(done) = self.tx.mass_apply(
            kind,
            name,
            values,
            ids,
            self.label_id,
            self.timestamp,
            origin.as_deref(),
        )?
        else {
            return Ok(None);
        };
        if let Some(n) = i64::try_from(done.changed.len()).ok().filter(|&n| n > 0) {
            self.first_op.get_or_insert(done.last_op - n + 1);
            self.flushed += n;
            self.chain_head = Some(done.last_op);
            if DECIDES_WATCHES.contains(&name) {
                self.effects.watch = true;
            }
        }
        Ok(Some(done.changed))
    }

    /// One operation on a field of many records: `one` writes each record's
    /// rows and its log entry, as a write to it alone would, while the store
    /// indexes the lot as one event when it can ([`WriteTxn::mass_begin`]).
    fn mass(
        &mut self,
        kind: MassKind,
        name: &str,
        values: Vec<Value>,
        uuids: &[Uuid],
        mut one: impl FnMut(&mut Self, Uuid) -> Result<bool>,
    ) -> Result<Vec<Uuid>> {
        let massed = self.tx.mass_begin(name, kind, &values)?;
        let mut changed = Vec::new();
        for &uuid in uuids {
            if one(self, uuid)? {
                changed.push(uuid);
            }
        }
        if massed {
            self.tx.mass_end(name, kind, values, uuids)?;
        }
        Ok(changed)
    }

    /// Flushes the remaining buffered operations, writes the final HEAD and
    /// commits the transaction.
    ///
    /// Each part is a phase of its own, inside the caller's `commit`, so a
    /// commit of seconds says which part took them (doc "What the slow log records").
    pub fn commit(mut self) -> Result<()> {
        metafolder_core::slowlog::timed("commit.checks", || -> Result<()> {
            self.check_forest_integrity()?;
            self.check_deferred_types()?;
            self.check_deferred_trees()
        })?;
        let log = metafolder_core::slowlog::phase("commit.log");
        if self.flushed == 0 && self.pending.is_empty() {
            // Nothing was written: drop the label it opened, leave HEAD alone.
            if self.opened_label {
                self.tx.drop_label(self.label_id)?;
            }
        } else {
            self.flush_pending()?;
            self.tx.set_head(self.chain_head)?;
            if let Some(first) = self.first_op {
                // In this transaction, deliberately: the trim then rides the
                // commit the write was going to pay for anyway. Never into
                // this write itself.
                self.tx.trim(self.retention, first)?;
            }
        }
        drop(log);
        self.tx.commit()
    }

    // ── Internals ────────────────────────────────────────────────────────────

    /// Refuses a write to a metarecord that does not exist.
    fn must_exist(&self, uuid: Uuid) -> Result<()> {
        if self.tx.exists(uuid)? {
            Ok(())
        } else {
            Err(DomainError::NotFound(format!("Metarecord not found: {uuid}")).into())
        }
    }

    /// A metarecord's version: a hash of its content, computed from its rows
    /// (doc "Metarecord version").
    fn current_version(&self, uuid: Uuid) -> Result<u64> {
        self.tx
            .version(uuid)?
            .ok_or_else(|| DomainError::NotFound(format!("Metarecord not found: {uuid}")).into())
    }

    /// Buffers one operation; the log rows are inserted in bulk, in batches
    /// of [`FLUSH_THRESHOLD`].
    fn log_op(
        &mut self,
        op_type: OpType,
        entity: Uuid,
        field_name: Option<&str>,
        before: Vec<FieldRow>,
        after: Vec<FieldRow>,
    ) -> Result<()> {
        self.observe_effects(op_type, &before, &after, entity);
        self.pending.push(NewOp {
            op_type,
            entity,
            field_name: field_name.map(str::to_string),
            before,
            after,
            reverts_op_id: self.reverting,
            origin: self.origin.clone(),
        });
        if self.pending.len() >= FLUSH_THRESHOLD {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Bulk-inserts the buffered `operation` and `op_snapshot` rows,
    /// advancing the running chain head.
    ///
    /// The store assigns the operation ids (from its sequence, never reused)
    /// and returns the last one, which becomes the chain head.
    fn flush_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        let last = self.tx.append_ops(self.label_id, self.timestamp, &pending)?;
        self.first_op.get_or_insert(last - pending.len() as i64 + 1);
        self.flushed += pending.len() as i64;
        self.chain_head = Some(last);
        Ok(())
    }

    /// The parents of `uuid`'s positions in `field`'s forest (`None` for a
    /// root), from its rows as this transaction sees them.
    fn tree_parents(&self, field: &str, uuid: Uuid) -> Result<Vec<Option<Uuid>>> {
        Ok(self
            .tx
            .rows_named(uuid, field)?
            .into_iter()
            .filter_map(|r| r.value.position().map(|(parent, _)| parent))
            .collect())
    }

    /// Enforces the "one value type per field name" invariant (doc "One value type per field name"):
    /// a field name carries a single non-`Nothing` value type repository-wide.
    /// `Nothing` is absence, not a type, so it is always allowed. A non-`Nothing`
    /// value whose type differs from the established one is rejected; changing a
    /// field's type is the dedicated `retype` operation (which bypasses this).
    /// At most one index seek (`idx_field_name_type`) per field name per revision:
    /// the established type is cached in [`Self::field_types`] after the first
    /// probe (or after this write establishes it).
    fn validate_value_type(&mut self, field_name: &str, value: &Value) -> Result<()> {
        if matches!(value, Value::Nothing) {
            return Ok(());
        }
        if let Some(deferred) = &mut self.deferred_types {
            // Checked once at commit instead, over the state this write
            // actually lands on.
            deferred.insert(field_name.to_string());
            return Ok(());
        }
        let new_type = rows::encode_value(value).value_type;

        if let Some(established) = self.field_types.get(field_name) {
            return if established == new_type {
                Ok(())
            } else {
                Err(Self::type_conflict(field_name, established, new_type))
            };
        }

        // Not cached yet: probe the DB once. An established differing type is a
        // conflict; otherwise this write fixes the type — cache it either way.
        if let Some(established) = self.tx.value_types(field_name)?.into_iter().next() {
            if established != new_type {
                return Err(Self::type_conflict(field_name, &established, new_type));
            }
        }
        self.field_types.insert(field_name.to_string(), new_type.to_string());
        Ok(())
    }

    fn type_conflict(field_name: &str, established: &str, attempted: &str) -> anyhow::Error {
        DomainError::BadRequest(format!(
            "field '{field_name}' has value type '{established}'; cannot write a \
             '{attempted}' value (use retype to change the field's type)"
        ))
        .into()
    }

    /// A metarecord holds at most one position in a forest (doc "One position per forest"):
    /// a `tree_ref` value joins `uuid`'s rows of
    /// `name` only if no row staying beside it — every one but the one holding `replaced` — is
    /// a position already, other than this very value (an append of it is a
    /// no-op). A `hierarchy_ref` may join other positions, all bearing its
    /// name (doc "Hierarchy references").
    fn validate_one_position(
        &self,
        uuid: Uuid,
        name: &str,
        value: &Value,
        replaced: Option<&Value>,
    ) -> Result<()> {
        if value.position().is_none() {
            return Ok(());
        }
        // Of the same type only: in the middle of a retype, the rows not
        // converted yet are no positions of the kind being written.
        let staying = self.tx.rows_named(uuid, name)?;
        let staying = staying
            .iter()
            .filter(|r| Some(&r.value) != replaced && r.value.type_str() == value.type_str())
            .map(|r| (name, &r.value));
        validate_one_position_each(uuid, staying.chain([(name, value)]))
    }

    /// For TreeRef values: the parent must be null (root) or an existing metarecord
    /// carrying a TreeRef of the same field name; the write must not create a
    /// cycle nor exceed [`MAX_TREE_DEPTH`] (doc "TreeRef forest").
    fn validate_tree_ref(&self, metarecord: Uuid, field_name: &str, value: &Value) -> Result<()> {
        let Some((parent, name)) = value.position() else {
            return Ok(());
        };
        // Checked even when the forest checks are deferred: it needs nothing
        // but the value.
        if !name.is_component(parent.is_none()) {
            return Err(DomainError::BadRequest(format!(
                "invalid name {:?} on '{field_name}': a name is one path component \
                 (no '/', no NUL, neither '.' nor '..', not empty)",
                name.display()
            ))
            .into());
        }
        if let Some(deferred) = &self.deferred_trees {
            deferred.borrow_mut().push((metarecord, field_name.to_string(), value.clone()));
            return Ok(());
        }
        self.check_tree_ref(metarecord, field_name, value)
    }

    /// [`Self::validate_tree_ref`]'s check, against the state as it stands.
    fn check_tree_ref(&self, metarecord: Uuid, field_name: &str, value: &Value) -> Result<()> {
        let Some((parent, _)) = value.position() else {
            return Ok(());
        };
        let Some(parent) = &parent else {
            return Ok(()); // Root node: nothing to check.
        };
        if *parent == metarecord {
            return Err(DomainError::BadRequest(format!(
                "TreeRef write would create a cycle on '{field_name}'"
            ))
            .into());
        }
        let parent_positions = self.tree_parents(field_name, *parent)?;
        if parent_positions.is_empty() {
            return Err(DomainError::BadRequest(format!(
                "invalid TreeRef parent {parent}: no such metarecord carrying a \
                 '{field_name}' TreeRef field"
            ))
            .into());
        }

        // Walk every ancestor chain (multi-map fields make this a DAG walk):
        // detect cycles through the metarecord being written and measure depth.
        let mut visited: HashSet<Uuid> = HashSet::new();
        let mut frontier = vec![*parent];
        let mut chain_len = 1; // The parent itself.
        loop {
            let mut next = Vec::new();
            for node in frontier {
                if node == metarecord {
                    return Err(DomainError::BadRequest(format!(
                        "TreeRef write would create a cycle on '{field_name}'"
                    ))
                    .into());
                }
                if !visited.insert(node) {
                    continue;
                }
                for gp in self.tree_parents(field_name, node)?.into_iter().flatten() {
                    next.push(gp);
                }
            }
            if next.is_empty() {
                break;
            }
            chain_len += 1;
            if chain_len >= MAX_TREE_DEPTH {
                return Err(DomainError::BadRequest(format!(
                    "TreeRef depth exceeds {MAX_TREE_DEPTH}"
                ))
                .into());
            }
            frontier = next;
        }
        // The new node sits one level below the deepest ancestor chain.
        if chain_len + 1 > MAX_TREE_DEPTH {
            return Err(
                DomainError::BadRequest(format!("TreeRef depth exceeds {MAX_TREE_DEPTH}")).into()
            );
        }
        Ok(())
    }
}

/// [`Writer::validate_one_position`] over the whole row set a write leaves on
/// `uuid` — a creation, an overwrite, a multi-valued set: at most one distinct
/// `tree_ref` value per name, and one name across a name's `hierarchy_ref`
/// values.
fn validate_one_position_each<'v>(
    uuid: Uuid,
    rows: impl Iterator<Item = (&'v str, &'v Value)>,
) -> Result<()> {
    let mut seen: HashMap<&str, &Value> = HashMap::new();
    for (name, value) in rows {
        match value {
            Value::TreeRef { .. } => match seen.insert(name, value) {
                Some(other) if other != value => return Err(second_position(uuid, name)),
                _ => {}
            },
            Value::HierarchyRef { name: node, .. } => match seen.insert(name, value) {
                Some(other) if other.position().is_some_and(|(_, n)| n != node) => {
                    return Err(second_name(uuid, name));
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

fn second_name(uuid: Uuid, name: &str) -> anyhow::Error {
    DomainError::BadRequest(format!(
        "metarecord {uuid} would bear two names in the '{name}' hierarchy: every \
         position of a node has one name (set all its positions to rename it)"
    ))
    .into()
}

fn second_position(uuid: Uuid, name: &str) -> anyhow::Error {
    DomainError::BadRequest(format!(
        "metarecord {uuid} would hold two positions in the '{name}' forest: a \
         metarecord has one position per forest (set the field to move it)"
    ))
    .into()
}

/// The outcome of [`Writer::append_field`]: the row was written, or the
/// metarecord already held the value — in which case the append was a no-op
/// (doc "No duplicate rows").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    Created,
    AlreadyPresent,
}

impl Appended {
    /// Whether a row was actually written.
    pub fn created(self) -> bool {
        matches!(self, Appended::Created)
    }
}

// ── Duplicate collapsing (doc "No duplicate rows") ───────────────

/// The values actually to be written: the first occurrence of each.
/// `["a", "b", "a"]` → `["a", "b"]`.
fn collapse_duplicates(values: &[Value]) -> Vec<Value> {
    let mut kept: Vec<Value> = Vec::with_capacity(values.len());
    for value in values {
        if !kept.contains(value) {
            kept.push(value.clone());
        }
    }
    kept
}

/// The fields to write for a whole-record write: the first occurrence of each
/// `(name, value)` pair, in the order given.
fn collapse_duplicate_fields(fields: Vec<Field>) -> Vec<Field> {
    let mut kept: Vec<Field> = Vec::with_capacity(fields.len());
    for f in fields {
        if !kept.iter().any(|k| k.name == f.name && k.value == f.value) {
            kept.push(f);
        }
    }
    kept
}

/// The refusal of a write naming a value the field does not hold.
fn no_such_value(uuid: Uuid, name: &str) -> anyhow::Error {
    DomainError::NotFound(format!("field '{name}' of metarecord {uuid} does not hold that value"))
        .into()
}
