//! The undo selection (spec-event-log "mf log undo"): deciding *what* a plain
//! "undo" should undo, and *how*.
//!
//! Undo is not "step HEAD back one revision". Between the change a user wants
//! back and the present sit the watcher's own revisions — a file touched, a
//! directory renamed — which are not the user's to undo: they record what the
//! filesystem did, and rewinding HEAD past them would unrecord facts that are
//! still true on disk. So undo looks for the newest revision *the user wrote*
//! and undoes that one:
//!
//! - if it is the revision HEAD sits in, a **rollback** — HEAD moves back over
//!   it and the log keeps no trace, which is what "undo" should feel like;
//! - otherwise a **revert** — its inverse is written at HEAD, leaving the
//!   watcher's revisions where they are.
//!
//! The two mechanisms are the daemon's (spec-event-log "Navigation" and
//! "Revert"); this module only chooses between them. It is pure: it reads a
//! `GET /log` body and answers, so the CLI (synchronous) and the GUI
//! (asynchronous) share one decision instead of each writing its own.

use serde_json::Value as Json;

/// The `revision.origin` the daemon stamps on a revision it wrote on the
/// filesystem's behalf (spec-event-log "Revision origin").
pub const ORIGIN_WATCHER: &str = "watcher";

/// Operation types the watcher records on the filesystem's behalf
/// (spec-event-log "Operation types"). Everything else is a client's write.
const WATCHER_OP_TYPES: [&str; 3] = ["file_deleted", "file_moved", "file_modified"];

/// Whether an operation type is one the watcher writes for the filesystem,
/// rather than one a user asked for.
pub fn is_watcher_op(op_type: &str) -> bool {
    WATCHER_OP_TYPES.contains(&op_type)
}

/// How many operations of history undo reads before deciding. A repository
/// whose last manual change is older than this asks again with
/// [`WIDE_WINDOW`] — two bounded reads rather than one unbounded one, which
/// after a large reconcile would be millions of rows.
pub const WINDOW: usize = 500;

/// The second, wider window (see [`WINDOW`]).
pub const WIDE_WINDOW: usize = 20_000;

/// One revision, as the undo selection needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRevision {
    pub id: i64,
    /// `Some("watcher")` for a revision the daemon wrote on the filesystem's
    /// behalf; `None` for a client's own write — and for every revision of a
    /// database written before the column existed, where the operation types
    /// are the fallback (spec-event-log "Revision origin").
    pub origin: Option<String>,
}

impl LogRevision {
    fn from_json(value: &Json) -> Option<Self> {
        Some(Self {
            id: value["id"].as_i64()?,
            origin: value["origin"].as_str().map(str::to_string),
        })
    }
}

/// One operation, as the undo selection needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogOp {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub rev_id: i64,
    pub op_type: String,
    /// The operation this one undid, when a revert wrote it.
    pub reverts_op_id: Option<i64>,
}

impl LogOp {
    fn from_json(value: &Json) -> Option<Self> {
        Some(Self {
            id: value["id"].as_i64()?,
            parent_id: value["parent_id"].as_i64(),
            rev_id: value["rev_id"].as_i64()?,
            op_type: value["op_type"].as_str()?.to_string(),
            reverts_op_id: value["reverts_op_id"].as_i64(),
        })
    }

    fn is_watcher(&self) -> bool {
        is_watcher_op(&self.op_type)
    }

    /// An unlogged write carries no snapshots, so neither mechanism can undo
    /// it (spec-event-log "Operation types").
    fn is_unknown(&self) -> bool {
        self.op_type == "unknown"
    }
}

/// What an undo should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoPlan {
    /// The user's newest change is the revision HEAD sits in: move HEAD back
    /// over it (`POST /rollback` with `{"prev_revision": true}`).
    Rollback { rev_id: i64 },
    /// The user's newest change has later work on top of it: write its inverse
    /// at HEAD (`POST /revert`). `ops` is `None` for the whole revision, and
    /// names what is left of it when some of its operations were reverted
    /// individually already.
    Revert { rev_id: i64, ops: Option<Vec<i64>> },
    /// Nothing of the user's is left to undo in the history that was read.
    Nothing,
}

impl UndoPlan {
    /// The revision the plan is about, if any.
    pub fn rev_id(&self) -> Option<i64> {
        match self {
            UndoPlan::Rollback { rev_id } | UndoPlan::Revert { rev_id, .. } => Some(*rev_id),
            UndoPlan::Nothing => None,
        }
    }

    /// One line saying what undo is about to do, and why that mechanism.
    pub fn describe(&self) -> String {
        match self {
            UndoPlan::Rollback { rev_id } => {
                format!("rollback: revision {rev_id} is the last thing written, so HEAD moves back over it")
            }
            UndoPlan::Revert { rev_id, ops } => {
                let what = match ops {
                    None => format!("revision {rev_id}"),
                    Some(ops) => format!("{} operation(s) of revision {rev_id}", ops.len()),
                };
                format!(
                    "revert: {what} has later work on top of it, so its inverse is written at HEAD"
                )
            }
            UndoPlan::Nothing => "nothing to undo".to_string(),
        }
    }
}

/// Decides what to undo, from a `GET /log` body (any `mode`).
///
/// Only HEAD's own ancestry is considered — the parent chain is walked from
/// HEAD — so a redo future left by an earlier rollback, or a divergent branch
/// a `tree` body carries, is ignored: those are not applied, and undoing them
/// would mean nothing.
pub fn plan_from_log(log: &Json) -> UndoPlan {
    let (ops, revisions) = read_log(log);
    plan(&ops, &revisions, log["head"].as_i64())
}

/// The operations and revisions of a `GET /log` body, skipping anything
/// malformed.
fn read_log(log: &Json) -> (Vec<LogOp>, Vec<LogRevision>) {
    let ops = log["operations"]
        .as_array()
        .map(|a| a.iter().filter_map(LogOp::from_json).collect())
        .unwrap_or_default();
    let revisions = log["revisions"]
        .as_array()
        .map(|a| a.iter().filter_map(LogRevision::from_json).collect())
        .unwrap_or_default();
    (ops, revisions)
}

/// Whether a [`UndoPlan::Nothing`] may be an artefact of how much log was read
/// rather than an empty history: the window came back full, so the user's last
/// change may lie just beyond it.
pub fn window_exhausted(log: &Json, limit: usize) -> bool {
    log["operations"].as_array().map(|a| a.len()).unwrap_or(0) >= limit
}

/// One revision of HEAD's line, with the operations of it that are on that
/// line and what has already happened to them.
struct RevisionOnLine<'a> {
    id: i64,
    members: Vec<&'a LogOp>,
    /// The operations of it that no *live* revert on this line has undone yet.
    left: Vec<i64>,
    /// Whether any of its operations undid an ordinary change, as opposed to
    /// undoing another undo (see [`Self::is_undo`]).
    undoes_a_change: bool,
}

impl RevisionOnLine<'_> {
    /// Whether the revision is itself an undo — any of its operations names the
    /// one it reverted. Asked *before* [`Self::is_daemon_written`], which it
    /// overrules: reverting a `file_moved` writes a `file_moved`, so a revert of
    /// the watcher's work looks exactly like the watcher's work by type.
    fn is_revert(&self) -> bool {
        self.members.iter().any(|op| op.reverts_op_id.is_some())
    }

    /// Whether it is an *undo*: a revert of an ordinary change. A revert whose
    /// target is itself a revert is a *redo* — it took an undo back — and redo
    /// must step over those, or pressing it twice would undo again.
    ///
    /// A target that lies outside the window counts as an ordinary change: the
    /// conservative reading, which at worst offers a redo that turns out to
    /// have nothing to take back.
    fn is_undo(&self) -> bool {
        self.undoes_a_change
    }

    /// Whether the daemon wrote it for the filesystem rather than a client
    /// asking for it — the revision says so (spec-event-log "Revision origin"),
    /// and on a database written before it could, the operation types are the
    /// fallback: sound (the watcher does write those types) without being exact
    /// (a file *arriving* is a `create_metarecord`). An unlogged write is
    /// lumped in here: it carries no snapshots, so neither mechanism reaches it.
    fn is_daemon_written(&self, watcher_written: &std::collections::HashSet<i64>) -> bool {
        watcher_written.contains(&self.id)
            || self.members.iter().all(|op| op.is_watcher() || op.is_unknown())
    }

    /// The root of the history as it stands: the first revision of a repository
    /// is the root metarecord the daemon writes at init, and after a prune the
    /// oldest revision left is a weak root whose predecessors are gone. There is
    /// no state before it to move HEAD back to.
    fn is_root(&self) -> bool {
        self.members.iter().any(|op| op.parent_id.is_none())
    }

    fn whole(&self) -> bool {
        self.left.len() == self.members.len()
    }

    /// `None` when the whole revision is still standing (target it by its id),
    /// `Some(ops)` when part of it was reverted operation by operation.
    fn remaining(&self) -> Option<Vec<i64>> {
        let mut left = self.left.clone();
        left.sort_unstable();
        (!self.whole()).then_some(left)
    }
}

/// HEAD's line of the history: the revisions on its ancestry, newest first.
struct Line<'a> {
    revisions: Vec<RevisionOnLine<'a>>,
    head_rev: i64,
    watcher_written: std::collections::HashSet<i64>,
}

impl<'a> Line<'a> {
    /// Walks the parent chain from `head`. Walking it — rather than trusting
    /// the order of the body — is what keeps a redo future left by an earlier
    /// rollback, and any divergent branch a `tree` body carries, out of it:
    /// those are not applied, and undoing them would mean nothing.
    fn of(ops: &'a [LogOp], revisions: &[LogRevision], head: i64) -> Option<Self> {
        let by_id: std::collections::HashMap<i64, &LogOp> =
            ops.iter().map(|op| (op.id, op)).collect();
        let mut ancestry: Vec<&LogOp> = Vec::new();
        let mut cursor = Some(head);
        while let Some(id) = cursor {
            let Some(op) = by_id.get(&id) else { break };
            ancestry.push(op);
            cursor = op.parent_id;
        }
        let head_rev = ancestry.first()?.rev_id;

        // What is undone *right now*. Not simply "named by some revert": a
        // revert that has itself been reverted (a redo) undid nothing any more,
        // and the change it had taken back stands again. So liveness is
        // resolved along the chain — and the walk gives the ancestry in
        // strictly decreasing id order, while a revert is always newer than
        // what it reverts, so one pass in that order settles every operation
        // after its own reverters.
        let mut reverters: std::collections::HashMap<i64, Vec<i64>> =
            std::collections::HashMap::new();
        for op in &ancestry {
            if let Some(target) = op.reverts_op_id {
                reverters.entry(target).or_default().push(op.id);
            }
        }
        let mut undone: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for op in &ancestry {
            let live =
                reverters.get(&op.id).is_some_and(|rs| rs.iter().any(|r| !undone.contains(r)));
            if live {
                undone.insert(op.id);
            }
        }

        let reverts: std::collections::HashSet<i64> =
            ancestry.iter().filter(|op| op.reverts_op_id.is_some()).map(|op| op.id).collect();

        let mut grouped: Vec<RevisionOnLine<'a>> = Vec::new();
        for op in ancestry {
            match grouped.iter_mut().find(|rev| rev.id == op.rev_id) {
                Some(rev) => rev.members.push(op),
                None => grouped.push(RevisionOnLine {
                    id: op.rev_id,
                    members: vec![op],
                    left: vec![],
                    undoes_a_change: false,
                }),
            }
        }
        for rev in &mut grouped {
            rev.left =
                rev.members.iter().filter(|op| !undone.contains(&op.id)).map(|op| op.id).collect();
            rev.undoes_a_change = rev
                .members
                .iter()
                .filter_map(|op| op.reverts_op_id)
                .any(|target| !reverts.contains(&target));
        }

        Some(Self {
            revisions: grouped,
            head_rev,
            watcher_written: revisions
                .iter()
                .filter(|rev| rev.origin.as_deref() == Some(ORIGIN_WATCHER))
                .map(|rev| rev.id)
                .collect(),
        })
    }

    /// Whether undoing `rev` can be a rollback: it is the revision HEAD sits
    /// in, all of it is still standing, and there is something behind it to
    /// move HEAD back to.
    fn rollbackable(&self, rev: &RevisionOnLine<'_>) -> bool {
        rev.id == self.head_rev && rev.whole() && !rev.is_root()
    }
}

/// The undo decision, over the operations and revisions of any log body.
pub fn plan(ops: &[LogOp], revisions: &[LogRevision], head: Option<i64>) -> UndoPlan {
    let Some(head) = head else { return UndoPlan::Nothing };
    let Some(line) = Line::of(ops, revisions, head) else { return UndoPlan::Nothing };

    for rev in &line.revisions {
        // The foundation of the history is not a change within it, and nothing
        // older is on this line anyway (`mf log revert <id>` still reaches it).
        if rev.is_root() {
            break;
        }
        // A revert is itself an undo: undoing it would be a redo, and the next
        // undo would put it back — the loop this rule exists to break. Redo is
        // where those belong ([`plan_redo`]).
        if rev.is_revert() {
            continue;
        }
        if rev.is_daemon_written(&line.watcher_written) {
            continue;
        }
        if rev.left.is_empty() {
            continue; // already undone, operation by operation
        }
        if line.rollbackable(rev) {
            return UndoPlan::Rollback { rev_id: rev.id };
        }
        return UndoPlan::Revert { rev_id: rev.id, ops: rev.remaining() };
    }
    UndoPlan::Nothing
}

// ── Redo ─────────────────────────────────────────────────────────────────────

/// What a redo should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedoPlan {
    /// HEAD is not at a tip: re-apply what a rollback undid by moving HEAD
    /// forward to this operation (`POST /rollback` with `{"id": …}`).
    Forward { op_id: i64, rev_id: i64 },
    /// The newest thing on the line is an undo and nothing has been written
    /// since: move HEAD back over it.
    Rollback { rev_id: i64 },
    /// The newest thing the user wrote is an undo, with the daemon's work on
    /// top of it: undo *it* in place — a revert of a revert.
    Revert { rev_id: i64, ops: Option<Vec<i64>> },
    /// There is no undo to take back.
    Nothing,
}

impl RedoPlan {
    pub fn rev_id(&self) -> Option<i64> {
        match self {
            RedoPlan::Forward { rev_id, .. }
            | RedoPlan::Rollback { rev_id }
            | RedoPlan::Revert { rev_id, .. } => Some(*rev_id),
            RedoPlan::Nothing => None,
        }
    }

    /// One line saying what redo is about to do, and why that mechanism.
    pub fn describe(&self) -> String {
        match self {
            RedoPlan::Forward { rev_id, .. } => {
                format!("forward: revision {rev_id} is ahead of HEAD, so HEAD moves back onto it")
            }
            RedoPlan::Rollback { rev_id } => format!(
                "rollback: revision {rev_id} undid something and is the last thing written, \
                 so HEAD moves back over it"
            ),
            RedoPlan::Revert { rev_id, ops } => {
                let what = match ops {
                    None => format!("revision {rev_id}"),
                    Some(ops) => format!("{} operation(s) of revision {rev_id}", ops.len()),
                };
                format!(
                    "revert: {what} undid something and has later work on top of it, \
                     so the undo is itself undone at HEAD"
                )
            }
            RedoPlan::Nothing => "nothing to redo".to_string(),
        }
    }
}

/// Decides what to redo, from a `GET /log` body.
///
/// The body must carry HEAD's *forward* continuation for the first case below
/// to be seen — `mode=active` or `mode=tree`, not `mode=linear`.
pub fn plan_redo_from_log(log: &Json) -> RedoPlan {
    let (ops, revisions) = read_log(log);
    plan_redo(&ops, &revisions, log["head"].as_i64())
}

/// The redo decision, the mirror of [`plan`]: where undo looks for the newest
/// change the user made and takes it back, redo looks for the newest undo and
/// takes *that* back.
///
/// Three cases, in order:
///
/// 1. HEAD is not at a tip — an earlier undo rolled it back and the operations
///    it unapplied are still there. Moving HEAD forward onto them is the exact
///    inverse, and it is what "redo" has always meant.
/// 2. HEAD sits in a revert and nothing has been written since: rolling back
///    over it removes the undo as cleanly as it was made.
/// 3. The newest revision the *user* wrote is a revert, with the daemon's own
///    revisions on top of it: reverting that revert puts the change back
///    without touching what the filesystem did in between.
///
/// Anything else — the newest thing you wrote is an ordinary change, not an
/// undo — is nothing to redo. Redo takes back an undo; it does not re-apply
/// arbitrary history.
pub fn plan_redo(ops: &[LogOp], revisions: &[LogRevision], head: Option<i64>) -> RedoPlan {
    if let Some((op_id, rev_id)) = forward_target(ops, head) {
        // Unless what lies ahead is an undo — which is what a redo by rollback
        // (case 2) leaves there. Moving forward onto it would undo again, and
        // redo and undo would trade the same revision back and forth.
        if !ops.iter().any(|op| op.rev_id == rev_id && op.reverts_op_id.is_some()) {
            return RedoPlan::Forward { op_id, rev_id };
        }
    }
    let Some(head) = head else { return RedoPlan::Nothing };
    let Some(line) = Line::of(ops, revisions, head) else { return RedoPlan::Nothing };

    for rev in &line.revisions {
        // A redo is a revert too; stepping over it is what keeps pressing redo
        // from undoing again.
        if rev.is_revert() && !rev.is_undo() {
            continue;
        }
        if rev.is_revert() {
            if rev.left.is_empty() {
                return RedoPlan::Nothing; // the undo has itself been undone
            }
            if line.rollbackable(rev) {
                return RedoPlan::Rollback { rev_id: rev.id };
            }
            return RedoPlan::Revert { rev_id: rev.id, ops: rev.remaining() };
        }
        // The daemon's revisions are stepped over: a file that moved after the
        // undo is exactly the case redo has to survive.
        if rev.is_daemon_written(&line.watcher_written) {
            continue;
        }
        // The newest thing the user wrote is not an undo.
        return RedoPlan::Nothing;
    }
    RedoPlan::Nothing
}

/// The operation to move HEAD forward to, and the revision it belongs to: the
/// last operation of the revision of HEAD's most recent child. `None` when HEAD
/// is at a tip. A `head` of `None` re-applies the first revision — the log has
/// been rolled back to the empty state.
pub fn forward_target(ops: &[LogOp], head: Option<i64>) -> Option<(i64, i64)> {
    let child = ops.iter().filter(|op| op.parent_id == head).max_by_key(|op| op.id)?;
    let last = ops.iter().filter(|op| op.rev_id == child.rev_id).map(|op| op.id).max()?;
    Some((last, child.rev_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `(id, rev_id, op_type)`, chained to the previous id. The first one
    /// continues from an operation outside the fixture (id 0) — what a bounded
    /// read of a real log looks like — so these cases are about the selection
    /// and not about the root of the history, which has its own test.
    fn ops(spec: &[(i64, i64, &str)]) -> Vec<LogOp> {
        let mut out: Vec<LogOp> = Vec::new();
        for (id, rev_id, op_type) in spec {
            let parent_id = Some(out.last().map(|op: &LogOp| op.id).unwrap_or(0));
            out.push(LogOp {
                id: *id,
                parent_id,
                rev_id: *rev_id,
                op_type: op_type.to_string(),
                reverts_op_id: None,
            });
        }
        out
    }

    fn reverting(mut op: LogOp, undid: i64) -> LogOp {
        op.reverts_op_id = Some(undid);
        op
    }

    #[test]
    fn an_empty_log_has_nothing_to_undo() {
        assert_eq!(plan(&[], &[], None), UndoPlan::Nothing);
        assert_eq!(plan(&ops(&[(1, 1, "set_field")]), &[], None), UndoPlan::Nothing);
    }

    #[test]
    fn a_manual_revision_at_head_is_rolled_back() {
        let log = ops(&[(1, 1, "create_metarecord"), (2, 2, "set_field")]);
        assert_eq!(plan(&log, &[], Some(2)), UndoPlan::Rollback { rev_id: 2 });
    }

    #[test]
    fn watcher_revisions_on_top_turn_the_undo_into_a_revert() {
        let log = ops(&[
            (1, 1, "set_field"),  // the user's change
            (2, 2, "file_moved"), // the watcher, twice
            (3, 3, "file_deleted"),
        ]);
        assert_eq!(plan(&log, &[], Some(3)), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn a_watcher_revision_is_never_itself_the_target() {
        let log = ops(&[(1, 1, "file_moved"), (2, 2, "file_deleted")]);
        assert_eq!(plan(&log, &[], Some(2)), UndoPlan::Nothing);
    }

    #[test]
    fn a_revision_mixing_a_manual_write_in_is_the_users() {
        // The whole revision goes back, watcher-typed operations included.
        let log = ops(&[(1, 1, "file_moved"), (2, 1, "set_field"), (3, 2, "file_deleted")]);
        assert_eq!(plan(&log, &[], Some(3)), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn an_unlogged_write_cannot_be_undone() {
        // The unlogged write is not a candidate, and it is not rolled back
        // *over* either: HEAD stays where it is and the revision below it is
        // reverted in place, exactly as for a watcher revision.
        let log = ops(&[(1, 1, "set_field"), (2, 2, "unknown")]);
        assert_eq!(plan(&log, &[], Some(2)), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn a_second_undo_walks_past_the_first_instead_of_cancelling_it() {
        // rev 1 and rev 2 are the user's; rev 3 is the watcher; rev 4 is the
        // revert the first undo wrote.
        let mut log = ops(&[
            (1, 1, "set_field"),
            (2, 2, "set_field"),
            (3, 3, "file_moved"),
            (4, 4, "set_field"),
        ]);
        log[3] = reverting(log[3].clone(), 2);
        assert_eq!(plan(&log, &[], Some(4)), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn a_revision_already_undone_is_skipped_even_at_head() {
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        // rev 2 is a revert (not a candidate) and rev 1 is already undone.
        assert_eq!(plan(&log, &[], Some(2)), UndoPlan::Nothing);
    }

    #[test]
    fn a_partly_reverted_revision_offers_what_is_left_of_it() {
        let mut log = ops(&[
            (1, 1, "set_field"),
            (2, 1, "set_field"),
            (3, 2, "set_field"), // reverted op 2 alone
        ]);
        log[2] = reverting(log[2].clone(), 2);
        // HEAD's revision is a revert, so the search lands on rev 1 — whose
        // op 2 is already undone, leaving op 1.
        assert_eq!(plan(&log, &[], Some(3)), UndoPlan::Revert { rev_id: 1, ops: Some(vec![1]) });
    }

    #[test]
    fn undo_stops_at_the_root_of_the_history() {
        // Op 1 has no parent: it is the repository's own first write (the root
        // metarecord), or what a prune left as the oldest revision.
        let mut log = ops(&[(1, 1, "create_metarecord")]);
        log[0].parent_id = None;
        assert_eq!(plan(&log, &[], Some(1)), UndoPlan::Nothing);
    }

    #[test]
    fn a_redo_future_ahead_of_head_is_not_undone() {
        // HEAD was rolled back to op 2; op 3 is still in the log, ahead of it.
        let log = ops(&[(1, 1, "create_metarecord"), (2, 2, "set_field"), (3, 3, "set_field")]);
        assert_eq!(plan(&log, &[], Some(2)), UndoPlan::Rollback { rev_id: 2 });
    }

    #[test]
    fn a_divergent_branch_is_not_undone() {
        // op 3 forks off op 1 — it is not on HEAD's line.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "file_moved")]);
        log.push(LogOp {
            id: 3,
            parent_id: Some(1),
            rev_id: 3,
            op_type: "set_field".into(),
            reverts_op_id: None,
        });
        assert_eq!(plan(&log, &[], Some(2)), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn a_revision_the_daemon_wrote_is_not_the_users_whatever_it_holds() {
        // A file arriving: recorded as a creation, like a user's, and told
        // apart only by the revision's origin.
        let log = ops(&[(1, 1, "set_field"), (2, 2, "create_metarecord")]);
        let revisions = [
            LogRevision { id: 1, origin: None },
            LogRevision { id: 2, origin: Some(ORIGIN_WATCHER.into()) },
        ];
        assert_eq!(plan(&log, &revisions, Some(2)), UndoPlan::Revert { rev_id: 1, ops: None });
        // Without the origin (an older database) the same log reads as the
        // user's — the fallback cannot see it.
        assert_eq!(plan(&log, &[], Some(2)), UndoPlan::Rollback { rev_id: 2 });
    }

    #[test]
    fn the_json_body_of_get_log_is_read_directly() {
        let log = json!({
            "head": 2,
            "operations": [
                {"id": 1, "parent_id": 0, "rev_id": 1, "op_type": "set_field",
                 "reverts_op_id": null},
                {"id": 2, "parent_id": 1, "rev_id": 2, "op_type": "file_moved",
                 "reverts_op_id": null},
            ],
            "revisions": [
                {"id": 1, "timestamp": 0, "label": null, "origin": null},
                {"id": 2, "timestamp": 0, "label": null, "origin": "watcher"},
            ],
        });
        assert_eq!(plan_from_log(&log), UndoPlan::Revert { rev_id: 1, ops: None });
        assert!(window_exhausted(&log, 2));
        assert!(!window_exhausted(&log, 3));
    }

    #[test]
    fn a_log_body_without_the_column_still_decides() {
        // A database written before `reverts_op_id` existed: every operation
        // reads as an ordinary write, which is what it was.
        let log = json!({
            "head": 1,
            "operations": [{"id": 1, "parent_id": 0, "rev_id": 1, "op_type": "set_field"}],
        });
        assert_eq!(plan_from_log(&log), UndoPlan::Rollback { rev_id: 1 });
    }

    // ── Redo ─────────────────────────────────────────────────────────────

    #[test]
    fn redo_moves_head_forward_when_there_is_something_ahead_of_it() {
        // An undo rolled HEAD back over revision 2; its operations are still
        // in the log, ahead of it.
        let log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 2, "set_field")]);
        assert_eq!(plan_redo(&log, &[], Some(1)), RedoPlan::Forward { op_id: 3, rev_id: 2 });
    }

    #[test]
    fn redo_from_the_empty_state_re_applies_the_first_revision() {
        let mut log = ops(&[(1, 1, "create_metarecord"), (2, 1, "set_field")]);
        log[0].parent_id = None;
        assert_eq!(plan_redo(&log, &[], None), RedoPlan::Forward { op_id: 2, rev_id: 1 });
    }

    #[test]
    fn redo_prefers_the_most_recent_branch() {
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field")]);
        log.push(LogOp {
            id: 3,
            parent_id: Some(1),
            rev_id: 3,
            op_type: "set_field".into(),
            reverts_op_id: None,
        });
        log.push(LogOp {
            id: 4,
            parent_id: Some(3),
            rev_id: 3,
            op_type: "set_field".into(),
            reverts_op_id: None,
        });
        assert_eq!(plan_redo(&log, &[], Some(1)), RedoPlan::Forward { op_id: 4, rev_id: 3 });
    }

    #[test]
    fn redo_rolls_back_an_undo_that_is_the_last_thing_written() {
        // The undo had to revert (something sat on top of the change), so HEAD
        // is at a tip; taking that revert back is a plain rollback.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "file_moved"), (3, 3, "set_field")]);
        log[2] = reverting(log[2].clone(), 1);
        assert_eq!(plan_redo(&log, &[], Some(3)), RedoPlan::Rollback { rev_id: 3 });
    }

    #[test]
    fn redo_reverts_the_revert_when_the_watcher_has_written_since() {
        // This is the case redo exists for: an undo, then the watcher moved a
        // file. HEAD cannot be rewound over the watcher's revision, so the undo
        // is itself undone in place.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "file_moved")]);
        log[1] = reverting(log[1].clone(), 1);
        let revisions = [
            LogRevision { id: 1, origin: None },
            LogRevision { id: 2, origin: None },
            LogRevision { id: 3, origin: Some(ORIGIN_WATCHER.into()) },
        ];
        assert_eq!(plan_redo(&log, &revisions, Some(3)), RedoPlan::Revert { rev_id: 2, ops: None });
    }

    #[test]
    fn redo_sees_a_revert_of_the_watchers_work_for_what_it_is() {
        // Undoing a `file_moved` writes a `file_moved`: by type alone the undo
        // is indistinguishable from the watcher's own revision, and redo would
        // step over the very thing it is looking for.
        let mut log = ops(&[(1, 1, "file_moved"), (2, 2, "file_moved"), (3, 3, "file_moved")]);
        log[2] = reverting(log[2].clone(), 1);
        let revisions = [
            LogRevision { id: 1, origin: Some(ORIGIN_WATCHER.into()) },
            LogRevision { id: 2, origin: Some(ORIGIN_WATCHER.into()) },
            LogRevision { id: 3, origin: None },
        ];
        assert_eq!(plan_redo(&log, &revisions, Some(3)), RedoPlan::Rollback { rev_id: 3 });
    }

    #[test]
    fn redo_does_not_move_forward_onto_an_undo() {
        // What a redo-by-rollback leaves behind: HEAD stepped back over the
        // revert (rev 3), which is now ahead of it. Moving forward onto it
        // would undo again, and the two would trade it back and forth.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "file_moved"), (3, 3, "set_field")]);
        log[2] = reverting(log[2].clone(), 1);
        assert_eq!(plan_redo(&log, &[], Some(2)), RedoPlan::Nothing);
        // An ordinary revision ahead of HEAD is re-applied as before.
        let plain = ops(&[(1, 1, "set_field"), (2, 2, "file_moved"), (3, 3, "set_field")]);
        assert_eq!(plan_redo(&plain, &[], Some(2)), RedoPlan::Forward { op_id: 3, rev_id: 3 });
    }

    #[test]
    fn there_is_nothing_to_redo_when_your_newest_change_is_not_an_undo() {
        let log = ops(&[(1, 1, "set_field"), (2, 2, "set_field")]);
        assert_eq!(plan_redo(&log, &[], Some(2)), RedoPlan::Nothing);
        assert_eq!(plan_redo(&[], &[], None), RedoPlan::Nothing);
    }

    #[test]
    fn an_undo_that_was_already_redone_is_not_redone_twice() {
        // rev 2 undid rev 1; rev 3 undid rev 2 (the redo). Pressing redo again
        // must not undo rev 2 a second time.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        log[2] = reverting(log[2].clone(), 2);
        assert_eq!(plan_redo(&log, &[], Some(3)), RedoPlan::Nothing);
    }

    #[test]
    fn undo_after_a_redo_takes_the_change_back_again() {
        // The pair has to compose: rev 2 undid rev 1, rev 3 undid rev 2.
        // Undo now finds rev 1 standing again, and rev 3 is at HEAD.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        log[2] = reverting(log[2].clone(), 2);
        assert_eq!(plan(&log, &[], Some(3)), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn redo_does_not_empty_the_history() {
        // A pruned log whose oldest remaining revision is a revert: taking it
        // back is a revert of it, never a rollback past the weak root.
        let mut log = ops(&[(1, 1, "set_field")]);
        log[0].parent_id = None;
        log[0] = reverting(log[0].clone(), 99);
        assert_eq!(plan_redo(&log, &[], Some(1)), RedoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn each_redo_plan_says_what_it_will_do() {
        assert!(RedoPlan::Forward { op_id: 3, rev_id: 7 }.describe().contains("revision 7"));
        assert!(RedoPlan::Rollback { rev_id: 7 }.describe().contains("revision 7"));
        assert!(RedoPlan::Revert { rev_id: 7, ops: Some(vec![1]) }
            .describe()
            .contains("1 operation"));
        assert_eq!(RedoPlan::Nothing.rev_id(), None);
    }

    #[test]
    fn the_json_body_drives_the_redo_too() {
        let log = json!({
            "head": 2,
            "operations": [
                {"id": 1, "parent_id": 0, "rev_id": 1, "op_type": "set_field"},
                {"id": 2, "parent_id": 1, "rev_id": 2, "op_type": "set_field",
                 "reverts_op_id": 1},
            ],
            "revisions": [
                {"id": 1, "origin": null},
                {"id": 2, "origin": null},
            ],
        });
        assert_eq!(plan_redo_from_log(&log), RedoPlan::Rollback { rev_id: 2 });
    }

    #[test]
    fn each_plan_says_what_it_will_do() {
        assert!(UndoPlan::Rollback { rev_id: 7 }.describe().contains("revision 7"));
        assert!(UndoPlan::Revert { rev_id: 7, ops: Some(vec![1, 2]) }
            .describe()
            .contains("2 operation"));
        assert!(UndoPlan::Revert { rev_id: 7, ops: None }.describe().contains("revision 7"));
        assert_eq!(UndoPlan::Nothing.rev_id(), None);
    }
}
