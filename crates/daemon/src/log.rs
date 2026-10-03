//! The logged write flow (doc "Log storage"). Every write to
//! the data tables goes through a [`Writer`], which records a revision, one
//! operation per atomic change with before/after snapshots, and keeps the
//! `log_head` pointer consistent with the data tables — all in one store
//! transaction.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use uuid::Uuid;

pub use metafolder_core::date::now_ms;
use metafolder_core::metarecord::{Field, FieldType, MetaRecord, Value};

use crate::error::DomainError;
use crate::rows::{self, FieldRow};
use crate::store::{Begin, Database, Log, NewOp, Restoration, Store, WriteTxn};
use crate::version;

/// The `revision.origin` of a revision the daemon writes on the filesystem's
/// behalf — the watcher's flush and the restoration replay (doc "Revisions and operations").
/// A client's own write leaves the column NULL.
pub const ORIGIN_WATCHER: &str = "watcher";

/// `revision.origin` for the metarecord deletion a trashing writes
/// (doc "POST /repos/:repo/metarecords/trash").
///
/// Deliberately *not* `watcher`: a trashing is a write the user asked for and
/// must stay undoable, and only `watcher` disqualifies a revision from being
/// undone (doc "Revisions and operations"). It is a distinct value only so
/// that a client walking a rollback knows the bytes of a deleted metarecord are
/// in the trash-bin rather than gone.
pub const ORIGIN_TRASH: &str = "trash";

/// Maximum depth of a TreeRef chain (doc "TreeRef forest").
pub const MAX_TREE_DEPTH: usize = 1000;

/// Operation types recorded in the log (doc "Revisions and operations").
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
    pub parent_id: Option<i64>,
    pub rev_id: i64,
    pub seq: i64,
    pub op_type: String,
    pub entity_uuid: Uuid,
    pub entity_version_before: Option<u64>,
    /// The version the entity held *after* this op. Redo restores it exactly;
    /// `None` on pre-migration rows, where forward application falls back to
    /// `entity_version_before + 1`.
    pub entity_version_after: Option<u64>,
    pub field_name: Option<String>,
    /// The operation this one undid, when a revert wrote it (doc "Revert").
    /// `None` for an ordinary write — and for every row of a
    /// database written before the column existed. Allowed to dangle: pruning
    /// may remove the operation it names.
    pub reverts_op_id: Option<i64>,
    /// The `origin` of the revision this operation belongs to (doc "Revisions and operations"),
    /// carried along so a reader never has to go back to
    /// the revision for it. `None` for a revision nothing stamped.
    ///
    /// It is what separates two operations the op type alone cannot: an
    /// ordinary `delete_metarecord` touches no file, while the one a *trashing*
    /// wrote has its bytes waiting in the trash-bin.
    pub origin: Option<String>,
}

/// What a bounded ancestor walk found. [`ancestry_ops_until`] flattens the two
/// failures into `None`; they are kept apart here for the caller that *widens*
/// its budget ([`linear_path`]), which has to tell "not far enough yet" from
/// "not on this chain at all" — the first is a reason to look again, the second
/// is the answer.
#[derive(Debug)]
pub enum Delta {
    /// The chain from `from` (inclusive) down to — but excluding — the anchor.
    Found(Vec<OpRow>),
    /// The budget ran out before the anchor was met; it may still be further
    /// down the chain.
    Budget,
    /// The walk reached the root of the history without meeting the anchor, so
    /// the anchor is not an ancestor of `from` at all.
    Unrelated,
}

/// The active line through `head` from its ancestry (HEAD-first) and every
/// operation: the ancestors root-first, then the branch below `head` that
/// leads to the newest operation (doc "Log endpoints"). Pure, so
/// every storage backend walks it the same way.
pub(crate) fn active_line_of(ancestry: Vec<OpRow>, all: Vec<OpRow>, head: i64) -> Vec<OpRow> {
    // Ancestry is HEAD→root; reverse to root→HEAD.
    let mut line = ancestry;
    line.reverse();

    // Build the child map from every operation to walk forward from HEAD.
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    for op in &all {
        if let Some(parent) = op.parent_id {
            children.entry(parent).or_default().push(op.id);
        }
    }
    // Subtree-max id per node. A child is always created after its parent
    // (parent_id < id), so processing ids in descending order visits children
    // before parents — one bottom-up pass, no recursion.
    let mut subtree_max: HashMap<i64, i64> = HashMap::new();
    let mut ids: Vec<i64> = all.iter().map(|o| o.id).collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    for id in ids {
        let mut m = id;
        if let Some(kids) = children.get(&id) {
            for &c in kids {
                m = m.max(*subtree_max.get(&c).unwrap_or(&c));
            }
        }
        subtree_max.insert(id, m);
    }

    // Walk forward from HEAD, always descending toward the largest reachable id.
    let by_id: HashMap<i64, &OpRow> = all.iter().map(|o| (o.id, o)).collect();
    let mut cur = head;
    while let Some(kids) = children.get(&cur) {
        let Some(&next) = kids.iter().max_by_key(|&&c| subtree_max.get(&c).copied().unwrap_or(c))
        else {
            break;
        };
        line.push((*by_id.get(&next).expect("child op present")).clone());
        cur = next;
    }
    line
}

// ── Navigation (doc "Navigation") ──────────────────────────────────

/// A rollback target, as given in the API request.
#[derive(Debug)]
pub enum Target {
    Id(i64),
    Timestamp(i64),
    Label(String),
    PrevRevision,
}

/// Resolves a target to an operation id; `Ok(None)` is the empty state.
pub fn resolve_target(log: &dyn Log, target: &Target) -> Result<Option<i64>> {
    let head = log.head()?;
    match target {
        Target::Id(id) => {
            log.op(*id)?
                .ok_or_else(|| DomainError::NotFound(format!("operation {id} not found")))?;
            Ok(Some(*id))
        }
        Target::Timestamp(t) => {
            let Some(head) = head else {
                anyhow::bail!("no operation found at or before timestamp {t} (empty history)");
            };
            log.ancestor_at_or_before(head, *t)?
                .map(Some)
                .with_context(|| format!("no operation found at or before timestamp {t}"))
        }
        Target::Label(label) => {
            let Some(head) = head else {
                return Err(DomainError::NotFound(format!(
                    "label '{label}' not found (empty history)"
                ))
                .into());
            };
            log.ancestor_labelled(head, label)?.map(Some).ok_or_else(|| {
                DomainError::NotFound(format!(
                    "label '{label}' not found on the HEAD ancestry path"
                ))
                .into()
            })
        }
        Target::PrevRevision => {
            let Some(head) = head else {
                anyhow::bail!("nothing to undo: the history is empty");
            };
            log.before_revision_of(head)
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct NavResult {
    pub previous_head: Option<i64>,
    pub new_head: Option<i64>,
    pub operations_unapplied: usize,
    pub operations_applied: usize,
}

/// Outcome of [`Writer::retype_field`]: how many rows were converted and the
/// metarecords whose values could not be converted and fell back to the
/// target's sentinel (deduped, sorted).
pub struct RetypeSummary {
    pub converted: usize,
    pub fallback_uuids: Vec<Uuid>,
}

/// Moves HEAD to `target` in one atomic transaction: inverse operations on
/// the path up to the LCA, forward operations down to the target.
pub fn navigate(store: &mut dyn Begin, target: Option<i64>) -> Result<NavResult> {
    let tx = store.begin_write()?;
    let previous_head = tx.head()?;
    if previous_head == target {
        return Ok(NavResult {
            previous_head,
            new_head: target,
            operations_unapplied: 0,
            operations_applied: 0,
        });
    }

    let (unapply, apply): (Vec<i64>, Vec<i64>) = match (previous_head, target) {
        (None, None) => (vec![], vec![]),
        (Some(head), None) => {
            // Empty state: every data row of this repository is removed (one repo
            // per database file, so every metarecord goes).
            let unapplied = tx.ancestry(head)?.len();
            tx.clear_metarecords()?;
            tx.set_head(None)?;
            tx.commit()?;
            return Ok(NavResult {
                previous_head,
                new_head: None,
                operations_unapplied: unapplied,
                operations_applied: 0,
            });
        }
        (None, Some(t)) => {
            let mut chain = tx.ancestry(t)?;
            chain.reverse(); // root → target
            (vec![], chain)
        }
        (Some(h), Some(t)) => {
            let h_anc = tx.ancestry(h)?;
            let h_set: HashSet<i64> = h_anc.iter().copied().collect();
            let t_anc = tx.ancestry(t)?;
            let lca = t_anc.iter().find(|id| h_set.contains(id)).copied();
            let unapply: Vec<i64> = h_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            let mut apply: Vec<i64> = t_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            apply.reverse(); // oldest first
            (unapply, apply)
        }
    };

    for op_id in &unapply {
        let op = tx.op(*op_id)?.context("operation vanished during navigation")?;
        apply_inverse(&*tx, &op)?;
    }
    for op_id in &apply {
        let op = tx.op(*op_id)?.context("operation vanished during navigation")?;
        apply_forward(&*tx, &op)?;
    }
    tx.set_head(target)?;
    tx.commit()?;

    Ok(NavResult {
        previous_head,
        new_head: target,
        operations_unapplied: unapply.len(),
        operations_applied: apply.len(),
    })
}

/// Direction of one step in a coordinated navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDir {
    /// Undo the operation (rollback toward an ancestor / the LCA).
    Inverse,
    /// Re-apply the operation (redo toward a descendant target).
    Forward,
}

/// The unapply and apply id lists from `head` toward `target`, one operation
/// at a time (unlike [`navigate`], the empty-state case is not bulk-deleted).
fn step_paths(
    log: &dyn Log,
    head: Option<i64>,
    target: Option<i64>,
) -> Result<(Vec<i64>, Vec<i64>)> {
    Ok(match (head, target) {
        (None, None) => (vec![], vec![]),
        (Some(h), None) => (log.ancestry(h)?, vec![]),
        (None, Some(t)) => {
            let mut chain = log.ancestry(t)?;
            chain.reverse();
            (vec![], chain)
        }
        (Some(h), Some(t)) => {
            let h_anc = log.ancestry(h)?;
            let h_set: HashSet<i64> = h_anc.iter().copied().collect();
            let t_anc = log.ancestry(t)?;
            let lca = t_anc.iter().find(|id| h_set.contains(id)).copied();
            let unapply: Vec<i64> = h_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            let mut apply: Vec<i64> = t_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            apply.reverse();
            (unapply, apply)
        }
    })
}

/// The first budget [`linear_path`] walks with, and the factor it widens by.
/// Small enough that the ordinary case — undoing the answer just given — reads
/// a handful of rows, and geometric so that a large delta still costs O(delta)
/// in total rather than one walk per widening.
const LINEAR_BUDGET: usize = 256;
const LINEAR_WIDEN: usize = 8;

/// The path between two operations that lie on one chain — the whole of a
/// rollback, and the whole of a redo — or `None` when they sit on diverging
/// branches and only the LCA answers.
///
/// This is the bounded half of [`nav_path`], and the reason it exists is the
/// *coordinated* navigation: it asks for the path again before every single
/// step, so a walk to the root of the log there is paid once per operation
/// undone. Going back over a tag applied to a folder of a thousand files then
/// costs a thousand walks over the whole history — the "back" key of a
/// classification walk (doc "Script sessions") taking minutes to answer.
///
/// The budget widens instead of being guessed: [`Delta::Budget`] means "look
/// further", [`Delta::Unrelated`] means "not this way", and only two
/// `Unrelated`s — the genuinely divergent case — fall back to the LCA.
fn linear_path(log: &dyn Log, head: i64, target: i64) -> Result<Option<Vec<(OpRow, NavDir)>>> {
    let (mut backward, mut forward) = (true, true);
    let mut budget = LINEAR_BUDGET;
    while backward || forward {
        if backward {
            match log.ops_until(head, target, budget)? {
                // The target is an ancestor: unapply everything above it,
                // newest first.
                Delta::Found(ops) => {
                    return Ok(Some(ops.into_iter().map(|op| (op, NavDir::Inverse)).collect()))
                }
                Delta::Unrelated => backward = false,
                Delta::Budget => {}
            }
        }
        if forward {
            match log.ops_until(target, head, budget)? {
                // The target is a descendant: re-apply the chain down to it,
                // oldest first.
                Delta::Found(mut ops) => {
                    ops.reverse();
                    return Ok(Some(ops.into_iter().map(|op| (op, NavDir::Forward)).collect()));
                }
                Delta::Unrelated => forward = false,
                Delta::Budget => {}
            }
        }
        budget = budget.saturating_mul(LINEAR_WIDEN);
    }
    Ok(None)
}

/// The full ordered list of operations to process to move HEAD from `head` to
/// `target`: each unapply op (most recent first) as [`NavDir::Inverse`], then
/// each apply op (oldest first) as [`NavDir::Forward`]. Empty when already at
/// the target (doc "Filesystem coordination").
pub fn nav_path(
    log: &dyn Log,
    head: Option<i64>,
    target: Option<i64>,
) -> Result<Vec<(OpRow, NavDir)>> {
    if head == target {
        return Ok(vec![]);
    }
    // Almost every navigation is along one chain — a rollback to an ancestor of
    // HEAD, a redo to a descendant of it — and that path can be read without
    // ever walking to the root of the log.
    if let (Some(h), Some(t)) = (head, target) {
        if let Some(path) = linear_path(log, h, t)? {
            return Ok(path);
        }
    }
    let (unapply, apply) = step_paths(log, head, target)?;
    let mut out = Vec::with_capacity(unapply.len() + apply.len());
    for id in unapply {
        out.push((log.op(id)?.context("operation vanished during navigation")?, NavDir::Inverse));
    }
    for id in apply {
        out.push((log.op(id)?.context("operation vanished during navigation")?, NavDir::Forward));
    }
    Ok(out)
}

/// A coordinated navigation's remaining steps, fixed when it starts.
///
/// Planning reads the path from HEAD to the target once; a step then takes
/// the next operation instead of reading the path again, so each step costs
/// the operation it applies — re-planning at every step made a navigation of
/// N operations cost N² (`tests/log_cost.rs`). The plan stays true because
/// nothing else writes while a navigation holds the rollback lock; a step
/// that finds HEAD elsewhere than the plan expects refuses rather than guess.
#[derive(Debug, Clone, Default)]
pub struct NavPlan {
    steps: std::collections::VecDeque<(i64, NavDir)>,
}

impl NavPlan {
    /// The steps from the current HEAD to `target`.
    pub fn new(log: &dyn Log, target: Option<i64>) -> Result<NavPlan> {
        let head = log.head()?;
        let steps = nav_path(log, head, target)?.into_iter().map(|(op, dir)| (op.id, dir));
        Ok(NavPlan { steps: steps.collect() })
    }

    /// How many steps are left.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The operation the next step applies, and in which direction.
    pub fn next(&self) -> Option<(i64, NavDir)> {
        self.steps.front().copied()
    }

    /// Applies the next step in one transaction and advances HEAD; see
    /// [`coordinated_step`]. Answers the new HEAD — the current one once the
    /// plan is done.
    pub fn step(&mut self, store: &mut dyn Begin, skip: bool) -> Result<Option<i64>> {
        let tx = store.begin_write()?;
        let head = tx.head()?;
        let Some((id, dir)) = self.steps.front().copied() else {
            return Ok(head);
        };
        let op = tx.op(id)?.context("operation vanished during navigation")?;
        let expected = match dir {
            NavDir::Inverse => Some(op.id),
            NavDir::Forward => op.parent_id,
        };
        if head != expected {
            anyhow::bail!(
                "the log moved during the navigation: HEAD is {head:?}, the plan expected \
                 {expected:?}"
            );
        }
        if skip {
            enqueue_restoration(&*tx, &op, dir)?;
        }
        let new_head = match dir {
            NavDir::Inverse => {
                apply_inverse(&*tx, &op)?;
                op.parent_id
            }
            NavDir::Forward => {
                apply_forward(&*tx, &op)?;
                Some(op.id)
            }
        };
        tx.set_head(new_head)?;
        tx.commit()?;
        self.steps.pop_front();
        Ok(new_head)
    }
}

/// Applies the *first* operation on the path from the current HEAD toward
/// `target` (one atomic step) and advances HEAD. Plans the whole path to take
/// its first step: a navigation of several steps plans once instead
/// ([`NavPlan`]). Returns the new HEAD. When `skip` is set and the
/// operation is a file op, a restoration entry is enqueued in
/// `pending_operation` (replayed as a new branch once the lock is released —
/// doc "Filesystem coordination").
pub fn coordinated_step(
    store: &mut dyn Begin,
    target: Option<i64>,
    skip: bool,
) -> Result<Option<i64>> {
    let mut plan = {
        // Read in a transaction dropped unwritten: a plain read of the store.
        let tx = store.begin_write()?;
        NavPlan::new(&*tx, target)?
    };
    plan.step(store, skip)
}

/// Enqueues the restoration operation for a skipped file op: a synthetic
/// metadata write replayed after the lock is released, correcting the metadata
/// to match the actual filesystem (doc "Filesystem coordination"). No filesystem check.
///
/// For `file_moved` the restoration *rewinds* `mfr_path` to the location the
/// file is recorded at **before this step** — i.e. the snapshot that is *not*
/// the one the step just applied. On an inverse step the step applied the
/// pre-move location (`is_new=0`), so we rewind to `is_new=1`; on a forward
/// (redo) step it applied the post-move location (`is_new=1`), so we rewind to
/// `is_new=0`. Taking the wrong side leaves the metadata where the (skipped,
/// hence not performed) move would have put it. See doc "Filesystem
/// coordination".
fn enqueue_restoration(tx: &dyn WriteTxn, op: &OpRow, dir: NavDir) -> Result<()> {
    let entity = op.entity_uuid;
    let restoration = match op.op_type.as_str() {
        // Rewind to the file's recorded location before this step (the side the
        // step did *not* apply): after it for an inverse, before it for a redo.
        "file_moved" => {
            let rows = tx.snapshots(op.id, dir == NavDir::Inverse)?;
            let Some(row) = rows.iter().find(|r| r.name == "mfr_path") else {
                return Ok(());
            };
            let Value::TreeRef { parent, name } = &row.value else { return Ok(()) };
            Restoration::SetPath { entity, parent: *parent, name: name.clone() }
        }
        // The file is gone: re-record the deletion.
        "file_deleted" => Restoration::ClearPath { entity },
        // The content changed: invalidate the hashes (size/mtime left stale).
        "file_modified" => Restoration::ClearHashes { entity },
        _ => return Ok(()),
    };
    tx.queue_restoration(&restoration)
}

/// Recomputes a metarecord's version from the rows it currently holds. This is
/// what navigation uses: restoring the rows restores the version with them, so
/// there is no second source of truth that could disagree with the content.
fn resync_version(tx: &dyn WriteTxn, uuid: Uuid) -> Result<()> {
    let rows = tx.rows(uuid)?;
    // A no-op when the step removed the metarecord: there is none to update.
    tx.set_version(uuid, version::of_rows(uuid, &rows))
}

/// Undoes one operation (doc "Navigation"). Field rows
/// are restored with their original primary keys.
fn apply_inverse(tx: &dyn WriteTxn, op: &OpRow) -> Result<()> {
    let entity = op.entity_uuid;
    match op.op_type.as_str() {
        "create_metarecord" => {
            tx.remove_metarecord(entity)?;
        }
        "delete_metarecord" => {
            tx.create_metarecord(entity, 0)?;
            for row in tx.snapshots(op.id, false)? {
                tx.insert_row(entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        // Whole-record set: replace the entire field set (all names).
        "set_metarecord" => {
            tx.delete_rows(entity, None)?;
            for row in tx.snapshots(op.id, false)? {
                tx.insert_row(entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        // All set-field-shaped operations (one field name, full replacement).
        "set_field" | "file_deleted" | "file_moved" | "file_modified" => {
            let field = op.field_name.as_deref().context("set-shaped op without field_name")?;
            tx.delete_rows(entity, Some(field))?;
            for row in tx.snapshots(op.id, false)? {
                tx.insert_row(entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "append_field" => {
            for row in tx.snapshots(op.id, true)? {
                tx.delete_row(row.id)?;
            }
        }
        "delete_field" => {
            for row in tx.snapshots(op.id, false)? {
                tx.insert_row(entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "unknown" => anyhow::bail!("cannot navigate across an 'unknown' operation (op {})", op.id),
        other => anyhow::bail!("unsupported op_type '{other}' in the log"),
    }
    // The version is not restored from the log: it is a function of the rows
    // this step has just put back (doc "Log storage"). `entity_version_before`/`after` are
    // provenance, and nothing
    // reads them to decide what to write.
    resync_version(tx, entity)
}

/// Replays one operation forward (redo).
fn apply_forward(tx: &dyn WriteTxn, op: &OpRow) -> Result<()> {
    let entity = op.entity_uuid;
    match op.op_type.as_str() {
        "create_metarecord" => {
            tx.create_metarecord(entity, 0)?;
            for row in tx.snapshots(op.id, true)? {
                tx.insert_row(entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "delete_metarecord" => {
            tx.remove_metarecord(entity)?;
        }
        "set_metarecord" => {
            tx.delete_rows(entity, None)?;
            for row in tx.snapshots(op.id, true)? {
                tx.insert_row(entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "set_field" | "file_deleted" | "file_moved" | "file_modified" => {
            let field = op.field_name.as_deref().context("set-shaped op without field_name")?;
            tx.delete_rows(entity, Some(field))?;
            for row in tx.snapshots(op.id, true)? {
                tx.insert_row(entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "append_field" => {
            for row in tx.snapshots(op.id, true)? {
                tx.insert_row(entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "delete_field" => {
            for row in tx.snapshots(op.id, false)? {
                tx.delete_row(row.id)?;
            }
        }
        "unknown" => anyhow::bail!("cannot navigate across an 'unknown' operation (op {})", op.id),
        other => anyhow::bail!("unsupported op_type '{other}' in the log"),
    }
    // The version is not restored from the log: it is a function of the rows
    // this step has just put back (doc "Log storage"). `entity_version_before`/`after` are
    // provenance, and nothing
    // reads them to decide what to write.
    resync_version(tx, entity)
}

// ── Pruning (doc "Pruning the log") ────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum PruneMode {
    Before,
    Linearize,
}

/// Permanently removes operations. The target must be an ancestor of HEAD
/// (or HEAD itself). Returns (pruned operations, pruned revisions).
pub fn prune(conn: &mut dyn Database, mode: PruneMode, target: i64) -> Result<(usize, usize)> {
    let head = conn.head()?.context("cannot prune an empty history")?;
    let head_path = conn.ancestry(head)?;
    if !head_path.contains(&target) {
        anyhow::bail!("prune target {target} must be an ancestor of HEAD (or HEAD itself)");
    }

    let ops = conn.all_ops()?;
    let mut children: HashMap<Option<i64>, Vec<i64>> = HashMap::new();
    for op in &ops {
        children.entry(op.parent_id).or_default().push(op.id);
    }
    let subtree = |roots: Vec<i64>| -> HashSet<i64> {
        let mut set = HashSet::new();
        let mut stack = roots;
        while let Some(id) = stack.pop() {
            if set.insert(id) {
                stack.extend(children.get(&Some(id)).into_iter().flatten().copied());
            }
        }
        set
    };

    let to_delete: HashSet<i64> = match mode {
        PruneMode::Before => {
            // Keep the target and everything below it; drop the rest.
            let keep = subtree(vec![target]);
            ops.iter().map(|o| o.id).filter(|id| !keep.contains(id)).collect()
        }
        PruneMode::Linearize => {
            // Drop branches diverging from the HEAD path strictly before the
            // target (the segment root→target becomes a straight line).
            let path_set: HashSet<i64> = head_path.iter().copied().collect();
            let target_pos = head_path.iter().position(|id| *id == target).unwrap();
            // head_path is head→root: nodes strictly before the target are
            // the ones after target_pos in that ordering.
            let mut branch_roots = Vec::new();
            for node in &head_path[target_pos + 1..] {
                for child in children.get(&Some(*node)).into_iter().flatten() {
                    if !path_set.contains(child) {
                        branch_roots.push(*child);
                    }
                }
            }
            subtree(branch_roots)
        }
    };

    let (_, revisions_before) = conn.counts()?;
    let tx = conn.begin_write()?;
    if matches!(mode, PruneMode::Before) {
        tx.detach_op(target)?;
    }
    // Children reference their parent (FK): delete newest-first, which is
    // child-before-parent since ids are monotonically increasing.
    let mut ordered: Vec<i64> = to_delete.iter().copied().collect();
    ordered.sort_unstable_by(|a, b| b.cmp(a));
    tx.delete_ops(&ordered)?;
    tx.drop_empty_revisions()?;
    tx.commit()?;
    let (_, revisions_after) = conn.counts()?;

    Ok((to_delete.len(), (revisions_before - revisions_after) as usize))
}

/// Buffered operations are flushed to the database once this many accumulate,
/// keeping the Writer's memory bounded on huge revisions (e.g. reconcile).
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
        if matches!(row.value, Value::TreeRef { .. }) && !out.iter().any(|c| c.field == row.name) {
            out.push(MovedCell { field: row.name.clone(), uuid: entity });
        }
    }
    out
}

/// What a committed revision obliges its caller to bring back in step — the
/// in-memory state a transaction cannot update itself (see
/// `RepoState::settle`). Read off the writer *before* `commit` consumes it.
#[derive(Debug, Default, Clone)]
pub struct WriteEffects {
    /// The cells the revision moved a position of, one entry per operation and
    /// field, in write order. Never truncated: a revision that changed the
    /// whole forest is exactly the one whose upkeep must not be guessed at.
    tree: Vec<MovedCell>,
    /// HEAD when the revision began: what the in-memory state settled by this
    /// revision must have described for a cheap upkeep to be enough.
    base_head: Option<i64>,
    /// Whether the revision wrote a field that decides which directories are
    /// watched.
    watch: bool,
}

impl WriteEffects {
    /// True if any `tree_ref` field row was created or removed.
    pub fn touches_tree(&self) -> bool {
        !self.tree.is_empty()
    }

    /// The cells the revision moved a position of, in write order.
    pub fn moved_cells(&self) -> &[MovedCell] {
        &self.tree
    }

    /// True if the revision wrote `mf_watch` / `mf_ignore` (eligibility) or
    /// `mfr_watch_exceeded` (the watch budget). A caller that keeps a live
    /// inotify watch set must refresh it: the set of watched directories may
    /// have changed.
    pub fn touches_watch(&self) -> bool {
        self.watch
    }

    /// HEAD when the revision began.
    pub fn base_head(&self) -> Option<i64> {
        self.base_head
    }
}

// ── Automatic retention (doc "Automatic retention") ────────────────

/// How much history the log keeps behind HEAD. Applied by [`Writer::commit`],
/// inside the write's own transaction: a trim is a range delete over the
/// oldest operations, which costs less than the append that triggered it and
/// adds no `fsync` of its own.
///
/// Deliberately *not* a [`prune`]: no `VACUUM`. In a steady state the freed
/// pages are what the next appends write into, so the database settles on a
/// plateau instead of being rewritten whole at every trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Revisions to keep. `0` = unlimited: nothing is ever dropped.
    pub revisions: u64,
    /// Never trim past a labelled revision: a named checkpoint is a floor, and
    /// the log grows past the limit rather than losing it.
    pub keep_labels: bool,
}

impl Retention {
    /// Keep everything. Deliberately not a `Default` impl: the default a
    /// repository actually writes with is the configured one
    /// (`DEFAULT_LOG_RETENTION_REVISIONS`), and a silent `Retention::default()`
    /// meaning "unlimited" would read as agreeing with it.
    pub const UNLIMITED: Self = Self { revisions: 0, keep_labels: false };

    /// How far past the limit the log is allowed to drift before a trim runs.
    /// Hysteresis, not sloppiness: trimming one revision per write would dirty
    /// the oldest (cold) pages of the log at every single commit, where one
    /// trim per `slack` revisions costs the same work spread over `slack`
    /// transactions that were going to write anyway.
    pub fn slack(revisions: u64) -> u64 {
        (revisions / 10).max(1)
    }

    pub(crate) fn enabled(&self) -> bool {
        self.revisions > 0
    }
}

/// A single logged write transaction. All changes made through one Writer
/// form one revision; commit is atomic. Dropping a Writer without committing
/// rolls everything back. After any method returns an error, the Writer must
/// be dropped (the whole revision is abandoned).
///
/// Data-table changes are applied immediately (later changes and lookups in
/// the same revision observe them); the log rows are buffered and inserted
/// in bulk, in batches of [`FLUSH_THRESHOLD`] operations.
pub struct Writer<'c> {
    tx: Box<dyn WriteTxn + 'c>,
    rev_id: i64,
    /// Parent of the next operation to flush: HEAD as of `begin`, then the
    /// last flushed operation.
    chain_head: Option<i64>,
    /// Number of operations already flushed to the database.
    flushed: i64,
    pending: Vec<NewOp>,
    /// Per-revision cache of each field name's established (non-`Nothing`) value
    /// type, populated lazily on the first checked write of a name. Collapses the
    /// per-write type probe to one DB seek per field name (a bulk reconcile/watcher
    /// revision writes the same ~8 reserved names across thousands of records).
    field_types: HashMap<String, String>,
    /// Field names whose type check this revision deferred to commit, and the
    /// reason it may: see [`Self::defer_type_checks`]. Empty for every ordinary
    /// write, which is checked as it happens.
    deferred_types: Option<HashSet<String>>,
    /// How much history to keep behind HEAD; applied on commit.
    retention: Retention,
    /// What this revision will oblige its caller to refresh, accumulated as the
    /// operations are recorded rather than read back off `pending` — which the
    /// flush above empties, so a long revision used to forget its early writes.
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
}

impl<'c> Writer<'c> {
    /// Opens a transaction and creates the revision row, keeping the whole
    /// history. Every write of a *loaded* repository goes through
    /// [`crate::state::RepoState::writer`] instead, which applies the
    /// repository's configured retention.
    pub fn begin(conn: &'c mut dyn Begin, label: Option<String>) -> Result<Self> {
        Self::begin_with_retention(conn, label, Retention::UNLIMITED)
    }

    /// Opens a transaction and creates the revision row, dropping the history
    /// that falls outside `retention` when the revision is committed.
    pub fn begin_with_retention(
        conn: &'c mut dyn Begin,
        label: Option<String>,
        retention: Retention,
    ) -> Result<Self> {
        let tx = conn.begin_write()?;
        let head = tx.head()?;
        let rev_id = tx.begin_revision(label.as_deref(), now_ms())?;
        Ok(Self {
            tx,
            rev_id,
            chain_head: head,
            flushed: 0,
            pending: Vec::new(),
            field_types: HashMap::new(),
            deferred_types: None,
            retention,
            effects: WriteEffects { base_head: head, ..WriteEffects::default() },
            tree_lost: Vec::new(),
            tree_lost_seen: HashMap::new(),
            reverting: None,
        })
    }

    pub fn rev_id(&self) -> i64 {
        self.rev_id
    }

    /// Marks this revision as written on the filesystem's behalf rather than at
    /// a client's request (doc "Revisions and operations"). The watcher's
    /// flush and the restoration replay set it; every other write leaves it
    /// unset, which is what "a client asked for this" means.
    ///
    /// It is the revision, not the operation types, that carries this: a file
    /// arriving is recorded as a `create_metarecord`, indistinguishable by type
    /// from a metarecord the user created.
    pub fn set_origin(&mut self, origin: &str) -> Result<()> {
        self.tx.set_revision_origin(self.rev_id, origin)
    }

    /// Read access to the transaction, for lookups that must observe the
    /// writes already applied.
    pub fn store(&self) -> &dyn Store {
        &*self.tx
    }

    /// Drops the queued restorations up to `up_to` in this revision's
    /// transaction — the one that re-records them, so a crash cannot replay
    /// them twice.
    pub fn drop_restorations(&self, up_to: i64) -> Result<()> {
        self.tx.drop_restorations(up_to)
    }

    /// Number of operations recorded so far in this revision.
    pub fn op_count(&self) -> i64 {
        self.flushed + self.pending.len() as i64
    }

    /// What this revision obliges its caller to bring back in step (watch
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
                && matches!(row.value, Value::TreeRef { .. })
                && !self
                    .tree_lost_seen
                    .get(row.name.as_str())
                    .is_some_and(|seen| seen.contains(&entity))
            {
                self.tree_lost.push((row.name.clone(), entity));
                self.tree_lost_seen.entry(row.name.clone()).or_default().insert(entity);
            }
        }
        const DECIDES_WATCHES: &[&str] =
            &["mf_watch", "mf_ignore", crate::eligibility::WATCH_EXCEEDED];
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
    /// Checked once, on the state the revision is about to commit, rather than
    /// per operation — deleting a whole subtree is legitimate even though the
    /// parent goes first, and only the committed state has to be a forest.
    ///
    /// Without it the children stayed, naming a node that no longer existed:
    /// *detached*, reachable by uuid but under no parent and in no roots map, so
    /// `GET /tree/roots` and path reconstruction disagreed about them ever after.
    /// Defers this revision's type checks to [`Self::commit`], where they are
    /// made against the state the revision lands on rather than against each
    /// intermediate one.
    ///
    /// Only a revert needs this, and only because of how it works: it undoes a
    /// set of operations one inverse at a time, newest to oldest, and the states
    /// in between need not be type-consistent even when the final one is.
    /// Undoing a `retype_field` is the case — after the last converted row's
    /// `append_field` is undone, the cell still holds the others in the *new*
    /// type. A rollback never meets this because navigation does not go through
    /// a `Writer` at all (see `apply_inverse`).
    ///
    /// Deferring is not skipping: [`Self::check_deferred_types`] refuses a
    /// revision that leaves two value types under one field name, which is what
    /// the per-write check was protecting.
    pub fn defer_type_checks(&mut self) {
        self.deferred_types.get_or_insert_with(Default::default);
        // The per-revision cache holds types probed under the eager rule; drop
        // it so nothing downstream reads a type this revision is about to move.
        self.field_types.clear();
    }

    /// Declares that the operations recorded from now on undo `op_id`, which
    /// each of them records as its `reverts_op_id` (doc "Revert").
    /// A revert sets it around each operation it walks; `None` restores the
    /// ordinary, unattributed write.
    pub fn reverting(&mut self, op_id: Option<i64>) {
        self.reverting = op_id;
    }

    /// The deferred check: every field name this revision wrote must carry a
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

    fn check_forest_integrity(&self) -> Result<()> {
        for (field_name, uuid) in &self.tree_lost {
            // Cheapest question first, and the one that is almost always "none":
            // a cell nobody is placed under has nothing to keep.
            let children = self.tx.children(field_name, *uuid)?;
            if children.is_empty() {
                continue;
            }
            let placed = self.tx.rows_named(*uuid, field_name)?;
            if placed.iter().any(|r| matches!(r.value, Value::TreeRef { .. })) {
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
        let version_before = self.current_version(uuid)?;
        self.tx.delete_rows(uuid, Some(name))?;
        self.log_op(op_type, uuid, Some(name), Some(version_before), before, vec![])?;
        self.field_types.remove(name); // rows removed: the type may have unlocked
        Ok(())
    }

    /// Creates a new metarecord owned by this repository.
    pub fn create_metarecord(&mut self, fields: Vec<Field>) -> Result<MetaRecord> {
        self.create_metarecord_with_uuid(Uuid::new_v4(), fields)
    }

    /// Like [`create_metarecord`] but at a caller-supplied UUID (sync
    /// bare-record creation, spec-sync). The UUID must not already exist — the
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
        self.tx.create_metarecord(uuid, version::base(uuid))?;

        let mut after = Vec::with_capacity(fields.len());
        let mut out_fields = Vec::with_capacity(fields.len());
        for f in fields {
            // Checked inside the loop so two rows of the same name with different
            // types within one create are rejected (the second sees the first).
            self.validate_value_type(&f.name, &f.value)?;
            let id = self.tx.insert_row(uuid, &f.name, &f.value, None)?;
            after.push(FieldRow { id, name: f.name.clone(), value: f.value.clone() });
            out_fields.push(Field { id: Some(id), ..f });
        }

        self.log_op(OpType::CreateRecord, uuid, None, None, vec![], after)?;
        let version = self.current_version(uuid)?;
        Ok(MetaRecord { uuid, version, fields: out_fields })
    }

    /// Deletes a metarecord and all its rows.
    ///
    /// One metarecord is never deleted: the repository root, the `mfr_path` root
    /// position (doc "TreeRef path conventions"). Every path
    /// hangs from it, and re-creating it is not a repair — a fresh metarecord
    /// gets a fresh uuid, while the whole forest keeps naming the deleted one.
    /// Recovery for a root deleted before this check existed is a rollback or a
    /// revert of the deletion, which restore it at its own uuid.
    pub fn delete_metarecord(&mut self, uuid: Uuid) -> Result<()> {
        let version = self
            .tx
            .version(uuid)?
            .ok_or_else(|| DomainError::NotFound(format!("Metarecord not found: {uuid}")))?;
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
        self.log_op(OpType::DeleteRecord, uuid, None, Some(version), before, vec![])?;
        Ok(())
    }

    /// Replaces the *entire* field set of an existing metarecord, keeping its
    /// UUID, in one `SetRecord` operation (before = all old fields, after = the
    /// new set) — the whole-record analogue of create/delete. Literal overwrite:
    /// every old row is dropped, including reserved ones not in `fields`.
    pub fn set_record(&mut self, uuid: Uuid, fields: Vec<Field>) -> Result<MetaRecord> {
        let fields = collapse_duplicate_fields(fields); // doc "No duplicate rows"
        validate_one_position_each(uuid, fields.iter().map(|f| (f.name.as_str(), &f.value)))?;
        let version_before = self.current_version(uuid)?; // errors NotFound if absent
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
            let id = self.tx.insert_row(uuid, &f.name, &f.value, None)?;
            after.push(FieldRow { id, name: f.name.clone(), value: f.value.clone() });
            out_fields.push(Field { id: Some(id), ..f });
        }
        self.log_op(OpType::SetRecord, uuid, None, Some(version_before), before, after)?;
        Ok(MetaRecord { uuid, version: version_before + 1, fields: out_fields })
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
        let version_before = self.current_version(uuid)?;
        let before = self.tx.rows_named(uuid, name)?;
        self.tx.delete_rows(uuid, Some(name))?;
        let cleared_to_nothing = matches!(value, Value::Nothing);
        let id = self.tx.insert_row(uuid, name, &value, None)?;
        let after = vec![FieldRow { id, name: name.to_string(), value }];
        self.log_op(op_type, uuid, Some(name), Some(version_before), before, after)?;
        if cleared_to_nothing {
            // The only remaining row is Nothing: the type may have unlocked.
            self.field_types.remove(name);
        }
        Ok(())
    }

    /// Replaces all rows for `(uuid, name)` with the given set of values
    /// (multi-map), in a single `SetField` operation. The existing `set_field`
    /// inverse/forward arms already restore *all* snapshot rows, so this needs
    /// no navigation change.
    pub fn set_field_multi(&mut self, uuid: Uuid, name: &str, values: Vec<Value>) -> Result<()> {
        self.set_field_multi_as(OpType::SetField, uuid, name, values).map(|_| ())
    }

    /// [`Self::set_field_multi`] under a chosen op type, returning one row id
    /// per *given* value, in order. A revert needs both: the watcher types when
    /// it undoes a file event (doc "What a revert writes"), and the ids to
    /// remap the row-scoped inverses that follow it — so a repeated value, which
    /// is collapsed to a single row (doc "No duplicate rows"),
    /// reports the id of the row that swallowed it rather than shifting the
    /// caller's pairing.
    pub fn set_field_multi_as(
        &mut self,
        op_type: OpType,
        uuid: Uuid,
        name: &str,
        values: Vec<Value>,
    ) -> Result<Vec<i64>> {
        validate_one_position_each(uuid, values.iter().map(|v| (name, v)))?;
        for value in &values {
            self.validate_tree_ref(uuid, name, value)?;
            self.validate_value_type(name, value)?;
        }
        // Each value is written once; a repeat points back at its first
        // occurrence so the returned ids still pair with `values`.
        let (kept, slot) = collapse_duplicates(&values);
        let version_before = self.current_version(uuid)?;
        let before = self.tx.rows_named(uuid, name)?;
        self.tx.delete_rows(uuid, Some(name))?;
        let cleared_to_nothing = values.iter().all(|v| matches!(v, Value::Nothing));
        let mut after = Vec::with_capacity(kept.len());
        for value in kept {
            let id = self.tx.insert_row(uuid, name, &value, None)?;
            after.push(FieldRow { id, name: name.to_string(), value });
        }
        let ids = slot.iter().map(|&i| after[i].id).collect();
        self.log_op(op_type, uuid, Some(name), Some(version_before), before, after)?;
        if cleared_to_nothing {
            // No non-Nothing row remains: the type may have unlocked.
            self.field_types.remove(name);
        }
        Ok(ids)
    }

    /// Appends one row without touching existing rows of that name.
    /// A value the metarecord already holds under that name is *not* appended
    /// again (doc "No duplicate rows"): nothing is written, nothing
    /// is logged, the version does not move, and the row that already holds it
    /// is reported as [`Appended::AlreadyPresent`].
    pub fn append_field(&mut self, uuid: Uuid, name: &str, value: Value) -> Result<Appended> {
        self.validate_one_position(uuid, name, &value, None)?;
        self.validate_tree_ref(uuid, name, &value)?;
        self.validate_value_type(name, &value)?;
        let twin = self.tx.rows_named(uuid, name)?.into_iter().find(|r| r.value == value);
        if let Some(existing) = twin.map(|r| r.id) {
            return Ok(Appended::AlreadyPresent(existing));
        }
        let version_before = self.current_version(uuid)?;
        let id = self.tx.insert_row(uuid, name, &value, None)?;
        let after = vec![FieldRow { id, name: name.to_string(), value }];
        self.log_op(OpType::AppendField, uuid, Some(name), Some(version_before), vec![], after)?;
        Ok(Appended::Created(id))
    }

    /// Replaces the single row identified by `field_id`, keeping its row id.
    /// Logged as a `delete_field` + `append_field` pair so that the inverse
    /// operations remain row-scoped (a `set_field` snapshot covers *all* rows
    /// of the name, which would clobber untouched sibling rows on rollback).
    pub fn replace_field(&mut self, uuid: Uuid, field_id: i64, value: Value) -> Result<()> {
        let old = self.get_owned_row(uuid, field_id)?;
        let name = old.name.clone();
        self.validate_one_position(uuid, &name, &value, Some(field_id))?;
        self.validate_tree_ref(uuid, &name, &value)?;
        self.validate_value_type(&name, &value)?;
        self.reject_duplicate(uuid, field_id, &name, &value)?;
        self.replace_owned_row(uuid, old, &name, value)
    }

    /// A by-id edit names one specific row, so turning it into the twin of a
    /// sibling is refused rather than silently dropping the row the caller just
    /// addressed (doc "No duplicate rows"). The row being edited is
    /// not its own twin.
    fn reject_duplicate(&self, uuid: Uuid, field_id: i64, name: &str, value: &Value) -> Result<()> {
        match self.twin_row(uuid, field_id, name, value)? {
            Some(id) => Err(DomainError::BadRequest(format!(
                "duplicate value for field '{name}': row {id} of {uuid} already holds it"
            ))
            .into()),
            None => Ok(()),
        }
    }

    /// The id of *another* row of `(uuid, name)` already holding `value`, if
    /// any — what makes the row `field_id` a duplicate.
    fn twin_row(
        &self,
        uuid: Uuid,
        field_id: i64,
        name: &str,
        value: &Value,
    ) -> Result<Option<i64>> {
        Ok(self
            .tx
            .rows_named(uuid, name)?
            .into_iter()
            .find(|r| r.id != field_id && &r.value == value)
            .map(|r| r.id))
    }

    /// Changes a field row's *name and/or value* in place, keeping its id — the
    /// by-id edit behind `mf field set`. The value type is validated against the
    /// *target* field name (a rename moves the row into that name's type group).
    pub fn rename_field(
        &mut self,
        uuid: Uuid,
        field_id: i64,
        new_name: &str,
        value: Value,
    ) -> Result<()> {
        let old = self.get_owned_row(uuid, field_id)?;
        self.validate_one_position(uuid, new_name, &value, Some(field_id))?;
        self.validate_tree_ref(uuid, new_name, &value)?;
        self.validate_value_type(new_name, &value)?;
        self.reject_duplicate(uuid, field_id, new_name, &value)?;
        self.replace_owned_row(uuid, old, new_name, value)
    }

    /// The logged delete+append core of [`Self::replace_field`], without the
    /// invariant checks. Shared with [`Self::retype_field`], which is itself the
    /// authority that changes a field's established type (so it must not be
    /// rejected by the per-write type check while converting row by row).
    /// `new_name` may differ from `old.name` (a by-id rename).
    fn replace_owned_row(
        &mut self,
        uuid: Uuid,
        old: FieldRow,
        new_name: &str,
        value: Value,
    ) -> Result<()> {
        let field_id = old.id;
        let v1 = self.current_version(uuid)?;
        self.tx.delete_row(field_id)?;
        self.log_op(
            OpType::DeleteField,
            uuid,
            Some(&old.name.clone()),
            Some(v1),
            vec![old.clone()],
            vec![],
        )?;

        let v2 = self.current_version(uuid)?;
        self.tx.insert_row(uuid, new_name, &value, Some(field_id))?;
        let after = vec![FieldRow { id: field_id, name: new_name.to_string(), value }];
        self.log_op(OpType::AppendField, uuid, Some(new_name), Some(v2), vec![], after)?;
        if new_name != old.name {
            // A rename can unlock the old name's type and lock the new one.
            self.field_types.remove(&old.name);
            self.field_types.remove(new_name);
        }
        Ok(())
    }

    /// Converts every non-`Nothing` row of field `name` to the type `to`,
    /// repository-wide, in this one revision (doc "Changing a field's type"). The target may be any
    /// type, including the reference variants.
    /// Row-scoped (each row keeps its id, so rollback restores it exactly) and
    /// bypasses the per-write type check (it *is* the type change). `Nothing`
    /// rows are left untouched (explicit absence is preserved). A `String →
    /// TreeRef` result that would violate the forest (missing parent, cycle,
    /// depth) is demoted to `Nothing` rather than aborting the whole retype, so
    /// one bad path never blocks the rest. Returns how many rows changed and the
    /// metarecords whose values fell back to the sentinel.
    pub fn retype_field(&mut self, name: &str, to: FieldType) -> Result<RetypeSummary> {
        // The field's established type changes here; drop any cached entry so a
        // later checked write in this revision re-probes.
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
                if matches!(new_value, Value::TreeRef { .. }) {
                    let valid = self
                        .validate_one_position(uuid, &row.name, &new_value, Some(row.id))
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
                if self.twin_row(uuid, row.id, &name, &new_value)?.is_some() {
                    self.delete_field(uuid, row.id)?;
                } else {
                    self.replace_owned_row(uuid, row, &name, new_value)?;
                }
                converted += 1;
            }
        }
        Ok(RetypeSummary { converted, fallback_uuids: fallback.into_iter().collect() })
    }

    /// Removes the single row identified by `field_id`.
    pub fn delete_field(&mut self, uuid: Uuid, field_id: i64) -> Result<()> {
        let old = self.get_owned_row(uuid, field_id)?;
        let version_before = self.current_version(uuid)?;
        self.tx.delete_row(field_id)?;
        self.log_op(
            OpType::DeleteField,
            uuid,
            Some(&old.name.clone()),
            Some(version_before),
            vec![old.clone()],
            vec![],
        )?;
        self.field_types.remove(&old.name); // a row removed: the type may have unlocked
        Ok(())
    }

    /// Deletes the given rows of `(uuid, name)` and logs them as *one*
    /// `DeleteField` operation (before = the rows, after = empty), so a bulk
    /// removal is one reversible log line per metarecord rather than one per row.
    /// Returns the number removed; logs nothing when `rows` is empty.
    fn delete_field_rows(&mut self, uuid: Uuid, name: &str, rows: Vec<FieldRow>) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let version_before = self.current_version(uuid)?;
        for row in &rows {
            self.tx.delete_row(row.id)?;
        }
        let removed = rows.len();
        self.log_op(OpType::DeleteField, uuid, Some(name), Some(version_before), rows, vec![])?;
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

    /// Flushes the remaining buffered operations, writes the final HEAD and
    /// commits the transaction.
    pub fn commit(mut self) -> Result<()> {
        self.check_forest_integrity()?;
        self.check_deferred_types()?;
        if self.flushed == 0 && self.pending.is_empty() {
            // Nothing was written: drop the empty revision, leave HEAD alone.
            self.tx.drop_revision(self.rev_id)?;
        } else {
            self.flush_pending()?;
            self.tx.set_head(self.chain_head)?;
            if let Some(head) = self.chain_head {
                // In this transaction, deliberately: the trim then rides the
                // commit the write was going to pay for anyway.
                self.tx.trim(self.retention, head)?;
            }
        }
        self.tx.commit()
    }

    // ── Internals ────────────────────────────────────────────────────────────

    /// The entity's version as it stands, i.e. the version this write is about
    /// to move away from. It is only *read* here: the new version is derived
    /// from the rows the write moves, in `log_op`, which is the single place a
    /// version is assigned.
    fn current_version(&self, uuid: Uuid) -> Result<u64> {
        self.tx
            .version(uuid)?
            .ok_or_else(|| DomainError::NotFound(format!("Metarecord not found: {uuid}")).into())
    }

    /// Fetches a field row, checking it belongs to the given metarecord.
    fn get_owned_row(&self, uuid: Uuid, field_id: i64) -> Result<FieldRow> {
        self.tx.rows(uuid)?.into_iter().find(|r| r.id == field_id).ok_or_else(|| {
            DomainError::NotFound(format!("Field {field_id} not found on metarecord {uuid}")).into()
        })
    }

    /// Buffers one operation; the log rows are inserted in bulk, in batches
    /// of [`FLUSH_THRESHOLD`].
    fn log_op(
        &mut self,
        op_type: OpType,
        entity: Uuid,
        field_name: Option<&str>,
        version_before: Option<u64>,
        before: Vec<FieldRow>,
        after: Vec<FieldRow>,
    ) -> Result<()> {
        // The version follows the rows this op moved: subtract the terms of
        // what it removed, add the terms of what it inserted (doc "Metarecord version").
        // This is the single place a version is assigned. `None`
        // when the op removed the metarecord itself — there is no row left to
        // carry a version, and nothing for a redo to restore.
        let version_after = match self.tx.version(entity)? {
            Some(current) => {
                let (add, sub) = version::delta(&before, &after);
                let assigned = version::apply(current, add, sub);
                self.tx.set_version(entity, assigned)?;
                Some(assigned)
            }
            None => None,
        };
        self.observe_effects(op_type, &before, &after, entity);
        self.pending.push(NewOp {
            op_type,
            entity,
            field_name: field_name.map(str::to_string),
            version_before,
            version_after,
            before,
            after,
            reverts_op_id: self.reverting,
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
        let last = self.tx.append_ops(self.rev_id, self.chain_head, self.flushed + 1, &pending)?;
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
            .filter_map(|r| match r.value {
                Value::TreeRef { parent, .. } => Some(parent),
                _ => None,
            })
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
            // Checked once at commit instead, over the state this revision
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
    /// `name` only if no row staying beside it — every one but `replaced` — is
    /// a position already, other than this very value (an append of it is a
    /// no-op).
    fn validate_one_position(
        &self,
        uuid: Uuid,
        name: &str,
        value: &Value,
        replaced: Option<i64>,
    ) -> Result<()> {
        if !matches!(value, Value::TreeRef { .. }) {
            return Ok(());
        }
        let taken = self.tx.rows_named(uuid, name)?.into_iter().any(|r| {
            Some(r.id) != replaced && matches!(r.value, Value::TreeRef { .. }) && r.value != *value
        });
        if taken {
            return Err(second_position(uuid, name));
        }
        Ok(())
    }

    /// For TreeRef values: the parent must be null (root) or an existing metarecord
    /// carrying a TreeRef of the same field name; the write must not create a
    /// cycle nor exceed [`MAX_TREE_DEPTH`] (doc "TreeRef forest").
    fn validate_tree_ref(&self, metarecord: Uuid, field_name: &str, value: &Value) -> Result<()> {
        let Value::TreeRef { parent, .. } = value else {
            return Ok(());
        };
        let Some(parent) = parent else {
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
/// `tree_ref` value per name.
fn validate_one_position_each<'v>(
    uuid: Uuid,
    rows: impl Iterator<Item = (&'v str, &'v Value)>,
) -> Result<()> {
    let mut seen: HashMap<&str, &Value> = HashMap::new();
    for (name, value) in rows {
        if !matches!(value, Value::TreeRef { .. }) {
            continue;
        }
        match seen.insert(name, value) {
            Some(other) if other != value => return Err(second_position(uuid, name)),
            _ => {}
        }
    }
    Ok(())
}

fn second_position(uuid: Uuid, name: &str) -> anyhow::Error {
    DomainError::BadRequest(format!(
        "metarecord {uuid} would hold two positions in the '{name}' forest: a \
         metarecord has one position per forest (set the field to move it)"
    ))
    .into()
}

/// The outcome of [`Writer::append_field`]: either the row it wrote, or the row
/// that already held the value — in which case the append was a no-op
/// (doc "No duplicate rows").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    Created(i64),
    AlreadyPresent(i64),
}

impl Appended {
    /// The id of the row holding the value, written now or already there.
    pub fn id(self) -> i64 {
        match self {
            Appended::Created(id) | Appended::AlreadyPresent(id) => id,
        }
    }

    /// Whether a row was actually written.
    pub fn created(self) -> bool {
        matches!(self, Appended::Created(_))
    }
}

// ── Duplicate collapsing (doc "No duplicate rows") ───────────────

/// Splits `values` into the values actually to be written (first occurrence of
/// each) and, for every given value, the index of the kept value that stands
/// for it. `(["a", "b", "a"])` → `(["a", "b"], [0, 1, 0])`.
fn collapse_duplicates(values: &[Value]) -> (Vec<Value>, Vec<usize>) {
    let mut kept: Vec<Value> = Vec::with_capacity(values.len());
    let mut slot = Vec::with_capacity(values.len());
    for value in values {
        match kept.iter().position(|k| k == value) {
            Some(i) => slot.push(i),
            None => {
                slot.push(kept.len());
                kept.push(value.clone());
            }
        }
    }
    (kept, slot)
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
