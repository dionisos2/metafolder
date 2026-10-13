//! The undo selection (doc "The undo selection", doc "Undo and redo by
//! label"): deciding *what* an undo or a redo takes back.
//!
//! Undo is not "step back one operation". Between the change a user wants
//! back and the present sit the watcher's own operations — a file touched, a
//! directory renamed — which are not the user's to undo: they record what the
//! filesystem did. So undo looks for the newest operation *the user wrote*
//! that still stands, and offers the labels it was written in — innermost
//! first: the command, the answer, the script run… (doc "Labels and
//! contexts"). Undoing the label chosen reverts its changes still standing
//! onto the current state (doc "Reverting onto the current state"), the
//! watcher's operations left where they are. Redo is its mirror over the
//! newest undo that still stands. There is one mechanism, and the log is a
//! sequence — nothing moves back over it (doc "The operation log").
//!
//! It is pure: it reads a `GET /log` body and answers, so the CLI
//! (synchronous) and the GUI (asynchronous) share one decision instead of each
//! writing its own. The revert itself narrows the label to what it takes
//! (`only`), in the daemon, which sees the whole label where the window may
//! see only its end.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value as Json};

/// The `origin` the daemon stamps on an operation it wrote on the
/// filesystem's behalf (doc "Operations and their origin").
pub const ORIGIN_WATCHER: &str = "watcher";

/// Operation types the watcher records on the filesystem's behalf
/// (doc "Operations and their origin"): the fallback when no origin says.
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

/// One operation, as the undo selection needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogOp {
    pub id: i64,
    /// Its innermost label (doc "Labels and contexts").
    pub label_id: i64,
    pub op_type: String,
    /// The operation this one undid, when a revert wrote it.
    pub reverts_op_id: Option<i64>,
    /// `Some("watcher")` for an operation the daemon wrote on the
    /// filesystem's behalf; `None` for a client's own write.
    pub origin: Option<String>,
}

impl LogOp {
    fn from_json(value: &Json) -> Option<Self> {
        Some(Self {
            id: value["id"].as_i64()?,
            label_id: value["label_id"].as_i64()?,
            op_type: value["op_type"].as_str()?.to_string(),
            reverts_op_id: value["reverts_op_id"].as_i64(),
            origin: value["origin"].as_str().map(str::to_string),
        })
    }

    /// The daemon's — by its origin, or failing that its type — or an
    /// unlogged write (no snapshots, nothing to invert): never the user's to
    /// undo or redo.
    fn is_daemons(&self) -> bool {
        self.origin.as_deref() == Some(ORIGIN_WATCHER)
            || (self.reverts_op_id.is_none() && is_watcher_op(&self.op_type))
            || self.op_type == "unknown"
    }
}

/// A label undo or redo offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label_id: i64,
    pub name: String,
}

/// What an undo or a redo would take back: the operation that decided it, and
/// the labels it was written in, innermost first. Undoing (redoing) any of
/// them is a revert of its changes (undos) still standing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub op: i64,
    pub candidates: Vec<Candidate>,
}

impl Selection {
    /// One line per candidate, `12  tag run`, innermost first.
    pub fn describe(&self) -> Vec<String> {
        self.candidates.iter().map(|c| format!("{}  {}", c.label_id, c.name)).collect()
    }
}

/// The revert an undo of `label` is: its changes still standing.
pub fn undo_target(label: i64) -> Json {
    json!({"label_id": label, "only": "changes"})
}

/// The revert a redo of `label` is: its undos still standing.
pub fn redo_target(label: i64) -> Json {
    json!({"label_id": label, "only": "undos"})
}

/// The labels of a `GET /log` body, by id: name and parent.
type Labels = HashMap<i64, (String, Option<i64>)>;

fn read_log(log: &Json) -> (Vec<LogOp>, Labels) {
    let ops = log["operations"]
        .as_array()
        .map(|a| a.iter().filter_map(LogOp::from_json).collect())
        .unwrap_or_default();
    let labels = log["labels"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    let name = l["name"].as_str().unwrap_or("").to_string();
                    Some((l["id"].as_i64()?, (name, l["parent"].as_i64())))
                })
                .collect()
        })
        .unwrap_or_default();
    (ops, labels)
}

/// The labels `label` is, innermost first: it, its parent, and so on — as
/// far as the body names them.
fn chain(label: i64, labels: &Labels) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut next = Some(label);
    while let Some(id) = next {
        if out.iter().any(|c| c.label_id == id) {
            break; // a cycle the daemon would never write
        }
        let (name, parent) = labels.get(&id).cloned().unwrap_or_default();
        out.push(Candidate { label_id: id, name });
        next = parent;
    }
    out
}

/// Whether a `None` may be an artefact of how much log was read rather than
/// an empty history: the window came back full, so the user's last change may
/// lie just beyond it.
pub fn window_exhausted(log: &Json, limit: usize) -> bool {
    log["operations"].as_array().map(|a| a.len()).unwrap_or(0) >= limit
}

/// The window's operations newest first, and which of them are undone right
/// now.
///
/// Not simply "named by some revert": a revert that has itself been reverted
/// (a redo) undid nothing any more, and the change it had taken back stands
/// again. So liveness is resolved along the chain — and a revert is always
/// newer than what it reverts, so one pass newest first settles every
/// operation after its own reverters.
fn window(ops: &[LogOp]) -> (Vec<&LogOp>, HashSet<i64>) {
    let mut newest_first: Vec<&LogOp> = ops.iter().collect();
    newest_first.sort_unstable_by_key(|op| std::cmp::Reverse(op.id));
    let mut reverters: HashMap<i64, Vec<i64>> = HashMap::new();
    for op in &newest_first {
        if let Some(target) = op.reverts_op_id {
            reverters.entry(target).or_default().push(op.id);
        }
    }
    let mut undone = HashSet::new();
    for op in &newest_first {
        if reverters.get(&op.id).is_some_and(|rs| rs.iter().any(|r| !undone.contains(r))) {
            undone.insert(op.id);
        }
    }
    (newest_first, undone)
}

/// What undo offers, from a `GET /log` body.
pub fn undo_from_log(log: &Json) -> Option<Selection> {
    let (ops, labels) = read_log(log);
    undo(&ops, &labels, log["first"].as_i64())
}

/// The undo decision: the newest operation the user wrote that is not itself
/// an undo or a redo and still stands. `first` is the log's first operation —
/// the root metarecord written at init, or the oldest a prune left — where
/// undo stops (`mf log revert` still reaches it).
fn undo(ops: &[LogOp], labels: &Labels, first: Option<i64>) -> Option<Selection> {
    let (newest_first, undone) = window(ops);
    for op in newest_first {
        if Some(op.id) == first {
            return None;
        }
        // A revert is itself an undo or a redo: undoing it would be a redo,
        // and the next undo would put it back — the loop this rule exists to
        // break. Redo is where those belong.
        if op.is_daemons() || op.reverts_op_id.is_some() || undone.contains(&op.id) {
            continue;
        }
        return Some(Selection { op: op.id, candidates: chain(op.label_id, labels) });
    }
    None
}

/// What redo offers, from a `GET /log` body.
pub fn redo_from_log(log: &Json) -> Option<Selection> {
    let (ops, labels) = read_log(log);
    redo(&ops, &labels)
}

/// The redo decision, the mirror of [`undo`]: the newest undo — a revert of
/// an ordinary change — still standing.
///
/// Newest first: the daemon's operations and the redos are stepped over, and
/// so are undos already redone — pressing redo again goes on to the undo
/// before. The newest *ordinary* change still standing ends the search: a new
/// change clears what could be redone, as in any editor. What it cleared is
/// still in the log, and an explicit revert of the undo reaches it.
fn redo(ops: &[LogOp], labels: &Labels) -> Option<Selection> {
    let (newest_first, undone) = window(ops);
    let reverts: HashSet<i64> =
        newest_first.iter().filter(|op| op.reverts_op_id.is_some()).map(|op| op.id).collect();
    for op in newest_first {
        if op.is_daemons() {
            continue;
        }
        match op.reverts_op_id {
            // A redo — a revert of a revert — is stepped over: that is what
            // keeps redo from undoing. A target outside the window counts as
            // an ordinary change: the conservative reading.
            Some(target) if reverts.contains(&target) => continue,
            Some(_) if undone.contains(&op.id) => continue, // an undo already redone
            Some(_) => {
                return Some(Selection { op: op.id, candidates: chain(op.label_id, labels) })
            }
            None if undone.contains(&op.id) => continue, // a change an undo holds back
            None => return None, // the newest thing the user wrote is a change
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(id, label_id, op_type)`.
    fn ops(spec: &[(i64, i64, &str)]) -> Vec<LogOp> {
        spec.iter()
            .map(|(id, label_id, op_type)| LogOp {
                id: *id,
                label_id: *label_id,
                op_type: op_type.to_string(),
                reverts_op_id: None,
                origin: None,
            })
            .collect()
    }

    fn reverting(mut op: LogOp, undid: i64) -> LogOp {
        op.reverts_op_id = Some(undid);
        op
    }

    fn watcher(mut op: LogOp) -> LogOp {
        op.origin = Some(ORIGIN_WATCHER.into());
        op
    }

    fn labels(nested: &[(i64, &str, Option<i64>)]) -> Labels {
        nested.iter().map(|(id, name, parent)| (*id, (name.to_string(), *parent))).collect()
    }

    /// The label undo would offer first, innermost.
    fn undo_label(log: &[LogOp]) -> Option<i64> {
        undo(log, &Labels::new(), BEFORE).map(|s| s.candidates[0].label_id)
    }

    fn redo_label(log: &[LogOp]) -> Option<i64> {
        redo(log, &Labels::new()).map(|s| s.candidates[0].label_id)
    }

    /// The log's first operation lies before the fixture — what a bounded read
    /// of a real log looks like.
    const BEFORE: Option<i64> = Some(0);

    #[test]
    fn an_empty_log_has_nothing_to_undo() {
        assert_eq!(undo(&[], &Labels::new(), None), None);
        assert_eq!(redo(&[], &Labels::new()), None);
    }

    #[test]
    fn the_newest_change_is_offered_with_every_label_it_is_in() {
        let log = ops(&[(1, 10, "set_field"), (2, 12, "set_field")]);
        let labels = labels(&[
            (12, "mf tag add x", Some(11)),
            (11, "answer 1", Some(10)),
            (10, "run", None),
        ]);
        let selection = undo(&log, &labels, BEFORE).unwrap();
        assert_eq!(selection.op, 2);
        let ids: Vec<i64> = selection.candidates.iter().map(|c| c.label_id).collect();
        assert_eq!(ids, [12, 11, 10], "innermost first");
        assert_eq!(selection.describe()[1], "11  answer 1");
    }

    #[test]
    fn the_watchers_operations_on_top_are_stepped_over() {
        let mut log =
            ops(&[(1, 1, "set_field"), (2, 2, "create_metarecord"), (3, 3, "file_moved")]);
        log[1] = watcher(log[1].clone());
        assert_eq!(undo_label(&log), Some(1), "an arrival is told apart by its origin alone");
    }

    #[test]
    fn an_undone_change_and_the_undo_itself_are_stepped_over() {
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "set_field")]);
        log[2] = reverting(log[2].clone(), 2);
        assert_eq!(undo_label(&log), Some(1));
    }

    #[test]
    fn undo_stops_at_the_first_operation_of_the_log() {
        let log = ops(&[(1, 1, "create_metarecord")]);
        assert_eq!(undo(&log, &Labels::new(), Some(1)), None);
    }

    #[test]
    fn the_json_body_of_get_log_is_read_directly() {
        let log = json!({
            "head": 2,
            "first": 0,
            "operations": [
                {"id": 1, "label_id": 1, "op_type": "set_field", "reverts_op_id": null},
                {"id": 2, "label_id": 2, "op_type": "create_metarecord", "reverts_op_id": null,
                 "origin": "watcher"},
            ],
            "labels": [{"id": 1, "name": "mf metarecord add", "parent": null}],
        });
        let selection = undo_from_log(&log).unwrap();
        assert_eq!(
            selection.candidates,
            [Candidate { label_id: 1, name: "mf metarecord add".into() }]
        );
        assert!(window_exhausted(&log, 2));
        assert!(!window_exhausted(&log, 3));
        assert_eq!(undo_target(1), json!({"label_id": 1, "only": "changes"}));
        assert_eq!(redo_target(1), json!({"label_id": 1, "only": "undos"}));
    }

    // ── Redo ─────────────────────────────────────────────────────────────

    #[test]
    fn redo_offers_the_newest_undo() {
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        assert_eq!(redo_label(&log), Some(2));
    }

    #[test]
    fn redo_survives_the_watcher_writing_after_the_undo() {
        // The case the history tree lost (doc "Why the log stops being a
        // tree"): an undo, then the watcher wrote. Its operation is stepped over.
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "file_moved")]);
        log[1] = reverting(log[1].clone(), 1);
        log[2] = watcher(log[2].clone());
        assert_eq!(redo_label(&log), Some(2));
    }

    #[test]
    fn redo_sees_a_revert_of_the_watchers_work_for_what_it_is() {
        // Undoing a `file_moved` writes a `file_moved`: by type alone the undo
        // is indistinguishable from the watcher's own; its revert says.
        let mut log = ops(&[(1, 1, "file_moved"), (2, 2, "file_moved"), (3, 3, "file_moved")]);
        log[0] = watcher(log[0].clone());
        log[1] = watcher(log[1].clone());
        log[2] = reverting(log[2].clone(), 1);
        assert_eq!(redo_label(&log), Some(3));
    }

    #[test]
    fn there_is_nothing_to_redo_when_your_newest_change_is_not_an_undo() {
        let log = ops(&[(1, 1, "set_field"), (2, 2, "set_field")]);
        assert_eq!(redo_label(&log), None);
    }

    #[test]
    fn an_undo_that_was_already_redone_is_not_redone_twice() {
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        log[2] = reverting(log[2].clone(), 2);
        assert_eq!(redo_label(&log), None);
    }

    #[test]
    fn two_undos_are_redone_one_after_the_other() {
        // 1, 2 changes; 3 undid 2, 4 undid 1; 5 redid 4. The next redo goes on
        // to 3 — the tree-era selection stopped at the redone undo.
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
        assert_eq!(redo_label(&log), Some(3));
    }

    #[test]
    fn a_new_change_clears_what_could_be_redone() {
        // Z, A; undo A (3); change M (4); undo M (5); undo Z (6).
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
        assert_eq!(undo_label(&log[..5]), Some(1));
        // Redo: Z comes back (undo it), then M, then nothing — A was cleared by M.
        assert_eq!(redo_label(&log), Some(6));
        log.push(reverting(ops(&[(7, 7, "set_field")])[0].clone(), 6));
        assert_eq!(redo_label(&log), Some(5));
        log.push(reverting(ops(&[(8, 8, "set_field")])[0].clone(), 5));
        assert_eq!(redo_label(&log), None);
    }

    #[test]
    fn undo_after_a_redo_takes_the_change_back_again() {
        let mut log = ops(&[(1, 1, "set_field"), (2, 2, "set_field"), (3, 3, "set_field")]);
        log[1] = reverting(log[1].clone(), 1);
        log[2] = reverting(log[2].clone(), 2);
        assert_eq!(undo_label(&log), Some(1));
    }
}
