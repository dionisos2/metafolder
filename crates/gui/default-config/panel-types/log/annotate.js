// log panel annotations: what each revision on the line *is*, so a revert can
// be aimed rather than guessed (spec-gui "Event log").
//
// Three things a revision list cannot show by revision number alone, and all
// three decide whether undoing it makes sense:
//
//   - the daemon wrote it for the filesystem (a file arrived, moved, went
//     away) — not yours to undo, and the undo selection skips it;
//   - it is itself an undo (it carries `reverts_op_id`);
//   - it has already been undone by a later revert.

/** Operation types the watcher records on the filesystem's behalf. */
const WATCHER_OP_TYPES = ['file_deleted', 'file_moved', 'file_modified'];

/**
 * One revision, as `GET /log` returns it.
 * @typedef {{id: number, origin?: string|null}} Rev
 *
 * One operation, as `GET /log` returns it.
 * @typedef {{id: number, rev_id: number, op_type: string,
 *            reverts_op_id?: number|null}} Op
 *
 * What this module works out about one revision.
 * @typedef {{watcher: boolean, isRevert: boolean, reverts: number[],
 *            undoneBy: number|null, undoneOps: number[],
 *            fullyUndone: boolean}} Marks
 */

/**
 * Annotates every revision of `revs` from the operations of `ops`.
 *
 * @param {Rev[]} revs @param {Op[]} ops
 * @returns {Map<number, Marks>}
 */
export function annotate(revs, ops) {
  /** @type {Map<number, Op[]>} */
  const byRev = new Map();
  /** @type {Map<number, number>} operation id → the revision holding it */
  const revOf = new Map();
  for (const op of ops) {
    const members = byRev.get(op.rev_id) ?? [];
    members.push(op);
    byRev.set(op.rev_id, members);
    revOf.set(op.id, op.rev_id);
  }

  // Who undid what: each revert names the operations it reverted, and those
  // operations' revisions are the ones marked as undone.
  /** @type {Map<number, {by: number, ops: number[]}>} */
  const undone = new Map();
  for (const op of ops) {
    const target = op.reverts_op_id ?? null;
    if (target === null) continue;
    const targetRev = revOf.get(target);
    if (targetRev === undefined) continue; // reverted outside the fetched window
    const entry = undone.get(targetRev) ?? { by: op.rev_id, ops: [] };
    entry.by = op.rev_id; // the most recent revert wins the label
    if (!entry.ops.includes(target)) entry.ops.push(target);
    undone.set(targetRev, entry);
  }

  /** @type {Map<number, Marks>} */
  const marks = new Map();
  for (const rev of revs) {
    const members = byRev.get(rev.id) ?? [];
    const reverted = undone.get(rev.id) ?? null;
    const reverts = members
      .map((op) => (op.reverts_op_id == null ? null : revOf.get(op.reverts_op_id) ?? null))
      .filter((id) => id !== null);
    marks.set(rev.id, {
      // The origin is the daemon's own word for it; the operation types are the
      // fallback for a database written before the column existed — sound (the
      // watcher writes those types) but not exact (an arrival is a creation).
      watcher:
        rev.origin === 'watcher' ||
        (members.length > 0 && members.every((op) => WATCHER_OP_TYPES.includes(op.op_type))),
      isRevert: members.some((op) => op.reverts_op_id != null),
      reverts: [...new Set(reverts)],
      undoneBy: reverted?.by ?? null,
      undoneOps: reverted?.ops ?? [],
      fullyUndone: reverted !== null && reverted.ops.length >= members.length,
    });
  }
  return marks;
}

/**
 * The `POST /revert` target for what is selected: one operation when the
 * selection is inside an expanded revision, the whole revision otherwise.
 *
 * @param {{kind: string, id: number, revId?: number}|null} selection
 * @returns {{rev_id: number}|{op_ids: number[]}|null}
 */
export function revertTarget(selection) {
  if (!selection) return null;
  return selection.kind === 'op' ? { op_ids: [selection.id] } : { rev_id: selection.id };
}
