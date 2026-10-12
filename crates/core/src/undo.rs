//! The undo selection (doc "The undo selection"): deciding *what* a plain
//! "undo" should undo.
//!
//! Undo is not "step back one revision". Between the change a user wants back
//! and the present sit the watcher's own revisions — a file touched, a
//! directory renamed — which are not the user's to undo: they record what the
//! filesystem did. So undo looks for the newest revision *the user wrote* that
//! still stands, and reverts it: its inverse is written onto the current state
//! (doc "Reverting onto the current state"), the watcher's revisions left
//! where they are. Redo is its mirror: it reverts the newest undo that still
//! stands. There is one mechanism, and the log is a sequence — nothing moves
//! back over it (doc "The operation log").
//!
//! It is pure: it reads a `GET /log` body and answers, so the CLI
//! (synchronous) and the GUI (asynchronous) share one decision instead of each
//! writing its own.

use serde_json::Value as Json;

/// The `revision.origin` the daemon stamps on a revision it wrote on the
/// filesystem's behalf (doc "Revisions and operations").
pub const ORIGIN_WATCHER: &str = "watcher";

/// Operation types the watcher records on the filesystem's behalf
/// (doc "Revisions and operations"). Everything else is a client's write.
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
    /// behalf; `None` for a client's own write (doc "Revisions and
    /// operations").
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
    pub rev_id: i64,
    pub op_type: String,
    /// The operation this one undid, when a revert wrote it.
    pub reverts_op_id: Option<i64>,
}

impl LogOp {
    fn from_json(value: &Json) -> Option<Self> {
        Some(Self {
            id: value["id"].as_i64()?,
            rev_id: value["rev_id"].as_i64()?,
            op_type: value["op_type"].as_str()?.to_string(),
            reverts_op_id: value["reverts_op_id"].as_i64(),
        })
    }

    fn is_watcher(&self) -> bool {
        is_watcher_op(&self.op_type)
    }

    /// An unlogged write carries no snapshots, so a revert cannot undo it
    /// (doc "Revisions and operations").
    fn is_unknown(&self) -> bool {
        self.op_type == "unknown"
    }
}

/// What an undo should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoPlan {
    /// Write the inverse of the user's newest standing change onto the current
    /// state (`POST /revert`). `ops` is `None` for the whole revision, and
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
            UndoPlan::Revert { rev_id, .. } => Some(*rev_id),
            UndoPlan::Nothing => None,
        }
    }

    /// One line saying what undo is about to do.
    pub fn describe(&self) -> String {
        match self {
            UndoPlan::Revert { rev_id, ops } => {
                format!("revert: {} — your newest change still standing", what(*rev_id, ops))
            }
            UndoPlan::Nothing => "nothing to undo".to_string(),
        }
    }
}

/// "revision 7", or "2 operation(s) of revision 7".
fn what(rev_id: i64, ops: &Option<Vec<i64>>) -> String {
    match ops {
        None => format!("revision {rev_id}"),
        Some(ops) => format!("{} operation(s) of revision {rev_id}", ops.len()),
    }
}

/// Decides what to undo, from a `GET /log` body.
pub fn plan_from_log(log: &Json) -> UndoPlan {
    let (ops, revisions) = read_log(log);
    plan(&ops, &revisions, log["first"].as_i64())
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

/// Whether a `Nothing` may be an artefact of how much log was read rather than
/// an empty history: the window came back full, so the user's last change may
/// lie just beyond it.
pub fn window_exhausted(log: &Json, limit: usize) -> bool {
    log["operations"].as_array().map(|a| a.len()).unwrap_or(0) >= limit
}

/// One revision of the window, with what has already happened to it.
struct Revision<'a> {
    id: i64,
    members: Vec<&'a LogOp>,
    /// Its operations that no *standing* revert has undone.
    left: Vec<i64>,
    /// Whether any of its operations undid an ordinary change, as opposed to
    /// undoing another undo (see [`Self::is_undo`]).
    undoes_a_change: bool,
}

impl Revision<'_> {
    /// Whether the revision is itself a revert — any of its operations names
    /// the one it reverted. Asked *before* [`Self::is_daemon_written`], which
    /// it overrules: reverting a `file_moved` writes a `file_moved`, so a
    /// revert of the watcher's work looks exactly like the watcher's work by
    /// type.
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
    /// asking for it — the revision says so (doc "Revisions and operations"),
    /// and the operation types are the fallback: sound (the watcher does write
    /// those types) without being exact (a file *arriving* is a
    /// `create_metarecord`). An unlogged write is lumped in here: it carries no
    /// snapshots, so a revert cannot reach it.
    fn is_daemon_written(&self, watcher_written: &std::collections::HashSet<i64>) -> bool {
        watcher_written.contains(&self.id)
            || self.members.iter().all(|op| op.is_watcher() || op.is_unknown())
    }

    /// Whether it holds the log's first operation: the root metarecord the
    /// daemon writes at init, or the oldest revision a prune left. Undo stops
    /// there (`mf log revert <id>` still reaches it).
    fn is_first(&self, first: Option<i64>) -> bool {
        first.is_some_and(|first| self.members.iter().any(|op| op.id == first))
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

/// The window's revisions, newest first, each with what still stands of it.
struct Window<'a> {
    revisions: Vec<Revision<'a>>,
    watcher_written: std::collections::HashSet<i64>,
}

impl<'a> Window<'a> {
    fn of(ops: &'a [LogOp], revisions: &[LogRevision]) -> Self {
        let mut newest_first: Vec<&LogOp> = ops.iter().collect();
        newest_first.sort_unstable_by_key(|op| std::cmp::Reverse(op.id));

        // What is undone *right now*. Not simply "named by some revert": a
        // revert that has itself been reverted (a redo) undid nothing any more,
        // and the change it had taken back stands again. So liveness is
        // resolved along the chain — and a revert is always newer than what it
        // reverts, so one pass newest first settles every operation after its
        // own reverters.
        let mut reverters: std::collections::HashMap<i64, Vec<i64>> =
            std::collections::HashMap::new();
        for op in &newest_first {
            if let Some(target) = op.reverts_op_id {
                reverters.entry(target).or_default().push(op.id);
            }
        }
        let mut undone: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for op in &newest_first {
            let live =
                reverters.get(&op.id).is_some_and(|rs| rs.iter().any(|r| !undone.contains(r)));
            if live {
                undone.insert(op.id);
            }
        }

        let reverts: std::collections::HashSet<i64> =
            newest_first.iter().filter(|op| op.reverts_op_id.is_some()).map(|op| op.id).collect();

        let mut grouped: Vec<Revision<'a>> = Vec::new();
        for op in newest_first {
            match grouped.iter_mut().find(|rev| rev.id == op.rev_id) {
                Some(rev) => rev.members.push(op),
                None => grouped.push(Revision {
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

        Self {
            revisions: grouped,
            watcher_written: revisions
                .iter()
                .filter(|rev| rev.origin.as_deref() == Some(ORIGIN_WATCHER))
                .map(|rev| rev.id)
                .collect(),
        }
    }
}

/// The undo decision, over the operations and revisions of a log window;
/// `first` is the log's first operation, where undo stops.
pub fn plan(ops: &[LogOp], revisions: &[LogRevision], first: Option<i64>) -> UndoPlan {
    let window = Window::of(ops, revisions);
    for rev in &window.revisions {
        // The foundation of the history is not a change within it.
        if rev.is_first(first) {
            break;
        }
        // A revert is itself an undo or a redo: undoing it would be a redo,
        // and the next undo would put it back — the loop this rule exists to
        // break. Redo is where those belong ([`plan_redo`]).
        if rev.is_revert() {
            continue;
        }
        if rev.is_daemon_written(&window.watcher_written) {
            continue;
        }
        if rev.left.is_empty() {
            continue; // already undone
        }
        return UndoPlan::Revert { rev_id: rev.id, ops: rev.remaining() };
    }
    UndoPlan::Nothing
}

// ── Redo ─────────────────────────────────────────────────────────────────────

/// What a redo should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedoPlan {
    /// Revert the newest undo that still stands — the change it took back
    /// comes back, onto the current state.
    Revert { rev_id: i64, ops: Option<Vec<i64>> },
    /// There is no undo to take back.
    Nothing,
}

impl RedoPlan {
    pub fn rev_id(&self) -> Option<i64> {
        match self {
            RedoPlan::Revert { rev_id, .. } => Some(*rev_id),
            RedoPlan::Nothing => None,
        }
    }

    /// One line saying what redo is about to do.
    pub fn describe(&self) -> String {
        match self {
            RedoPlan::Revert { rev_id, ops } => {
                format!("revert: {} — the newest undo still standing", what(*rev_id, ops))
            }
            RedoPlan::Nothing => "nothing to redo".to_string(),
        }
    }
}

/// Decides what to redo, from a `GET /log` body.
pub fn plan_redo_from_log(log: &Json) -> RedoPlan {
    let (ops, revisions) = read_log(log);
    plan_redo(&ops, &revisions)
}

/// The redo decision, the mirror of [`plan`]: where undo looks for the newest
/// change the user made and takes it back, redo looks for the newest undo and
/// takes *that* back.
///
/// Newest first: the daemon's revisions and the redos are stepped over, and so
/// are undos already redone — pressing redo again goes on to the undo before.
/// The newest *ordinary* change still standing ends the search: a new change
/// clears what could be redone, as in any editor. What it cleared is still in
/// the log, and an explicit revert of the undo reaches it.
pub fn plan_redo(ops: &[LogOp], revisions: &[LogRevision]) -> RedoPlan {
    let window = Window::of(ops, revisions);
    for rev in &window.revisions {
        if rev.is_revert() && !rev.is_undo() {
            continue; // a redo: stepping over it is what keeps redo from undoing
        }
        if rev.is_revert() {
            if rev.left.is_empty() {
                continue; // an undo already redone
            }
            return RedoPlan::Revert { rev_id: rev.id, ops: rev.remaining() };
        }
        // The daemon's revisions are stepped over: a file that moved after the
        // undo is exactly the case redo has to survive.
        if rev.is_daemon_written(&window.watcher_written) {
            continue;
        }
        if rev.left.is_empty() {
            continue; // an ordinary change an undo still holds back
        }
        // The newest thing the user wrote is a change, not an undo.
        return RedoPlan::Nothing;
    }
    RedoPlan::Nothing
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `(id, rev_id, op_type)`.
    fn ops(spec: &[(i64, i64, &str)]) -> Vec<LogOp> {
        spec.iter()
            .map(|(id, rev_id, op_type)| LogOp {
                id: *id,
                rev_id: *rev_id,
                op_type: op_type.to_string(),
                reverts_op_id: None,
            })
            .collect()
    }

    fn reverting(mut op: LogOp, undid: i64) -> LogOp {
        op.reverts_op_id = Some(undid);
        op
    }

    /// The log's first operation lies before the fixture — what a bounded read
    /// of a real log looks like.
    const BEFORE: Option<i64> = Some(0);

    #[test]
    fn an_empty_log_has_nothing_to_undo() {
        assert_eq!(plan(&[], &[], None), UndoPlan::Nothing);
    }

    #[test]
    fn the_newest_manual_revision_is_reverted() {
        let log = ops(&[(1, 1, "create_metarecord"), (2, 2, "set_field")]);
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Revert { rev_id: 2, ops: None });
    }

    #[test]
    fn watcher_revisions_on_top_are_stepped_over() {
        let log = ops(&[
            (1, 1, "set_field"),  // the user's change
            (2, 2, "file_moved"), // the watcher, twice
            (3, 3, "file_deleted"),
        ]);
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn a_watcher_revision_is_never_itself_the_target() {
        let log = ops(&[(1, 1, "file_moved"), (2, 2, "file_deleted")]);
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Nothing);
    }

    #[test]
    fn a_revision_mixing_a_manual_write_in_is_the_users() {
        // The whole revision goes back, watcher-typed operations included.
        let log = ops(&[(1, 1, "file_moved"), (2, 1, "set_field"), (3, 2, "file_deleted")]);
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn an_unlogged_write_cannot_be_undone() {
        let log = ops(&[(1, 1, "set_field"), (2, 2, "unknown")]);
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Revert { rev_id: 1, ops: None });
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
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn a_revision_already_undone_is_skipped() {
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        // rev 2 is a revert (not a candidate) and rev 1 is already undone.
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Nothing);
    }

    #[test]
    fn a_partly_reverted_revision_offers_what_is_left_of_it() {
        let mut log = ops(&[
            (1, 1, "set_field"),
            (2, 1, "set_field"),
            (3, 2, "set_field"), // reverted op 2 alone
        ]);
        log[2] = reverting(log[2].clone(), 2);
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Revert { rev_id: 1, ops: Some(vec![1]) });
    }

    #[test]
    fn undo_stops_at_the_first_operation_of_the_log() {
        // Op 1 is the log's first: the root metarecord written at init, or
        // what a prune left as the oldest revision.
        let log = ops(&[(1, 1, "create_metarecord")]);
        assert_eq!(plan(&log, &[], Some(1)), UndoPlan::Nothing);
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
        assert_eq!(plan(&log, &revisions, BEFORE), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn the_json_body_of_get_log_is_read_directly() {
        let log = json!({
            "head": 2,
            "first": 0,
            "operations": [
                {"id": 1, "rev_id": 1, "op_type": "set_field", "reverts_op_id": null},
                {"id": 2, "rev_id": 2, "op_type": "file_moved", "reverts_op_id": null},
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

    // ── Redo ─────────────────────────────────────────────────────────────

    #[test]
    fn redo_reverts_the_newest_undo() {
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        assert_eq!(plan_redo(&log, &[]), RedoPlan::Revert { rev_id: 2, ops: None });
    }

    #[test]
    fn redo_survives_the_watcher_writing_after_the_undo() {
        // The case the history tree lost (doc "Why the log stops being a
        // tree"): an undo, then the watcher wrote. Its revision is stepped over.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "file_moved")]);
        log[1] = reverting(log[1].clone(), 1);
        let revisions = [
            LogRevision { id: 1, origin: None },
            LogRevision { id: 2, origin: None },
            LogRevision { id: 3, origin: Some(ORIGIN_WATCHER.into()) },
        ];
        assert_eq!(plan_redo(&log, &revisions), RedoPlan::Revert { rev_id: 2, ops: None });
    }

    #[test]
    fn redo_sees_a_revert_of_the_watchers_work_for_what_it_is() {
        // Undoing a `file_moved` writes a `file_moved`: by type alone the undo
        // is indistinguishable from the watcher's own revision.
        let mut log = ops(&[(1, 1, "file_moved"), (2, 2, "file_moved"), (3, 3, "file_moved")]);
        log[2] = reverting(log[2].clone(), 1);
        let revisions = [
            LogRevision { id: 1, origin: Some(ORIGIN_WATCHER.into()) },
            LogRevision { id: 2, origin: Some(ORIGIN_WATCHER.into()) },
            LogRevision { id: 3, origin: None },
        ];
        assert_eq!(plan_redo(&log, &revisions), RedoPlan::Revert { rev_id: 3, ops: None });
    }

    #[test]
    fn there_is_nothing_to_redo_when_your_newest_change_is_not_an_undo() {
        let log = ops(&[(1, 1, "set_field"), (2, 2, "set_field")]);
        assert_eq!(plan_redo(&log, &[]), RedoPlan::Nothing);
        assert_eq!(plan_redo(&[], &[]), RedoPlan::Nothing);
    }

    #[test]
    fn an_undo_that_was_already_redone_is_not_redone_twice() {
        // rev 2 undid rev 1; rev 3 undid rev 2 (the redo). Pressing redo again
        // finds rev 1 standing: a change, so nothing.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        log[2] = reverting(log[2].clone(), 2);
        assert_eq!(plan_redo(&log, &[]), RedoPlan::Nothing);
    }

    #[test]
    fn two_undos_are_redone_one_after_the_other() {
        // rev 1, rev 2 changes; rev 3 undid rev 2, rev 4 undid rev 1; rev 5
        // redid rev 4. The next redo goes on to rev 3 — the tree-era selection
        // stopped at the redone undo and answered nothing.
        let mut log = ops(&[
            (1, 1, "set_field"),
            (2, 2, "set_field"),
            (3, 3, "set_field"),
            (4, 4, "set_field"),
            (5, 5, "set_field"),
        ]);
        log[2] = reverting(log[2].clone(), 2);
        log[3] = reverting(log[3].clone(), 1);
        log[4] = reverting(log[4].clone(), 4);
        assert_eq!(plan_redo(&log, &[]), RedoPlan::Revert { rev_id: 3, ops: None });
    }

    #[test]
    fn a_new_change_clears_what_could_be_redone() {
        // Z, A; undo A (rev 3); change M (rev 4); undo M (rev 5); undo Z (rev 6).
        let mut log = ops(&[
            (1, 1, "set_field"), // Z
            (2, 2, "set_field"), // A
            (3, 3, "set_field"), // undo A
            (4, 4, "set_field"), // M
            (5, 5, "set_field"), // undo M
            (6, 6, "set_field"), // undo Z
        ]);
        log[2] = reverting(log[2].clone(), 2);
        log[4] = reverting(log[4].clone(), 4);
        log[5] = reverting(log[5].clone(), 1);
        assert_eq!(plan(&log[..5], &[], BEFORE), UndoPlan::Revert { rev_id: 1, ops: None });
        // Redo: Z comes back (undo it), then M, then nothing — A was cleared by M.
        assert_eq!(plan_redo(&log, &[]), RedoPlan::Revert { rev_id: 6, ops: None });
        log.push(reverting(ops(&[(7, 7, "set_field")])[0].clone(), 6));
        assert_eq!(plan_redo(&log, &[]), RedoPlan::Revert { rev_id: 5, ops: None });
        log.push(reverting(ops(&[(8, 8, "set_field")])[0].clone(), 5));
        assert_eq!(plan_redo(&log, &[]), RedoPlan::Nothing);
    }

    #[test]
    fn undo_after_a_redo_takes_the_change_back_again() {
        // The pair has to compose: rev 2 undid rev 1, rev 3 undid rev 2.
        // Undo now finds rev 1 standing again.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        log[2] = reverting(log[2].clone(), 2);
        assert_eq!(plan(&log, &[], BEFORE), UndoPlan::Revert { rev_id: 1, ops: None });
    }

    #[test]
    fn each_plan_says_what_it_will_do() {
        assert!(UndoPlan::Revert { rev_id: 7, ops: Some(vec![1, 2]) }
            .describe()
            .contains("2 operation"));
        assert!(UndoPlan::Revert { rev_id: 7, ops: None }.describe().contains("revision 7"));
        assert_eq!(UndoPlan::Nothing.rev_id(), None);
        assert!(RedoPlan::Revert { rev_id: 7, ops: None }.describe().contains("revision 7"));
        assert_eq!(RedoPlan::Nothing.rev_id(), None);
    }

    #[test]
    fn the_json_body_drives_the_redo_too() {
        let log = json!({
            "head": 2,
            "operations": [
                {"id": 1, "rev_id": 1, "op_type": "set_field"},
                {"id": 2, "rev_id": 2, "op_type": "set_field", "reverts_op_id": 1},
            ],
            "revisions": [{"id": 1, "origin": null}, {"id": 2, "origin": null}],
        });
        assert_eq!(plan_redo_from_log(&log), RedoPlan::Revert { rev_id: 2, ops: None });
    }
}
