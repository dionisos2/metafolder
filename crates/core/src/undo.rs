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
    let ops: Vec<LogOp> = log["operations"]
        .as_array()
        .map(|a| a.iter().filter_map(LogOp::from_json).collect())
        .unwrap_or_default();
    let revisions: Vec<LogRevision> = log["revisions"]
        .as_array()
        .map(|a| a.iter().filter_map(LogRevision::from_json).collect())
        .unwrap_or_default();
    plan(&ops, &revisions, log["head"].as_i64())
}

/// Whether a [`UndoPlan::Nothing`] may be an artefact of how much log was read
/// rather than an empty history: the window came back full, so the user's last
/// change may lie just beyond it.
pub fn window_exhausted(log: &Json, limit: usize) -> bool {
    log["operations"].as_array().map(|a| a.len()).unwrap_or(0) >= limit
}

/// The decision itself, over the operations and revisions of any log body.
pub fn plan(ops: &[LogOp], revisions: &[LogRevision], head: Option<i64>) -> UndoPlan {
    let Some(head) = head else { return UndoPlan::Nothing };

    // HEAD's ancestry, newest first. Walking the parent chain rather than
    // trusting the order of the body: ids only increase along one branch.
    let by_id: std::collections::HashMap<i64, &LogOp> = ops.iter().map(|op| (op.id, op)).collect();
    let mut ancestry: Vec<&LogOp> = Vec::new();
    let mut cursor = Some(head);
    while let Some(id) = cursor {
        let Some(op) = by_id.get(&id) else { break };
        ancestry.push(op);
        cursor = op.parent_id;
    }
    if ancestry.is_empty() {
        return UndoPlan::Nothing;
    }

    // What the reverts on this line have already undone. A revert on an
    // abandoned branch undid nothing that is applied now, so it does not count.
    let undone: std::collections::HashSet<i64> =
        ancestry.iter().filter_map(|op| op.reverts_op_id).collect();
    let head_rev = ancestry[0].rev_id;

    let watcher_written: std::collections::HashSet<i64> = revisions
        .iter()
        .filter(|rev| rev.origin.as_deref() == Some(ORIGIN_WATCHER))
        .map(|rev| rev.id)
        .collect();

    // Revisions in ancestry order (newest first), each with its operations.
    let mut grouped: Vec<(i64, Vec<&LogOp>)> = Vec::new();
    for op in &ancestry {
        match grouped.iter_mut().find(|(rev_id, _)| *rev_id == op.rev_id) {
            Some((_, members)) => members.push(op),
            None => grouped.push((op.rev_id, vec![op])),
        }
    }

    for (rev_id, members) in grouped {
        // The foundation of the history is not a change within it: the first
        // revision of a repository is the root metarecord the daemon writes at
        // init, and after a prune the oldest revision left is a weak root whose
        // predecessors are gone. Either way there is no state to go back to, so
        // undo stops here (`mf log revert <id>` still reaches it).
        if members.iter().any(|op| op.parent_id.is_none()) {
            break;
        }
        // A revert is itself an undo: undoing it would be a redo, and the next
        // undo would put it back — the loop this rule exists to break.
        if members.iter().any(|op| op.reverts_op_id.is_some()) {
            continue;
        }
        // Nothing here is the user's: the daemon recorded what the filesystem
        // did — the revision says so, and on a database written before it could,
        // the operation types are the fallback (a file arriving is a
        // `create_metarecord`, so the fallback is a sound rule and not an exact
        // one). An unlogged write cannot be undone at all.
        if watcher_written.contains(&rev_id)
            || members.iter().all(|op| op.is_watcher() || op.is_unknown())
        {
            continue;
        }
        let mut left: Vec<i64> =
            members.iter().filter(|op| !undone.contains(&op.id)).map(|op| op.id).collect();
        if left.is_empty() {
            continue; // already undone, operation by operation
        }
        // The whole revision is still standing and HEAD is in it: a rollback
        // undoes it and leaves no trace, which a revert cannot.
        let whole = left.len() == members.len();
        if rev_id == head_rev && whole {
            return UndoPlan::Rollback { rev_id };
        }
        left.sort_unstable();
        return UndoPlan::Revert { rev_id, ops: (!whole).then_some(left) };
    }
    UndoPlan::Nothing
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
