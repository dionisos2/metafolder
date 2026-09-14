// log panel selection movement: revisions are listed in reverse
// chronological order, so delta +1 moves down the list (older).
//
// The rows are whatever the panel says they are: revisions alone, or revisions
// with the operations of the expanded one inlined between them. The caller
// keys the rows (`rev:12`, `op:34`) so the two kinds cannot collide; this
// module only walks the list it is handed.

/**
 * A selectable row, of which only the id matters here.
 * @typedef {{id: number|string}} Row
 */

/**
 * Returns the row id selected after moving by `delta` rows from `selectedId`,
 * clamped to the list; the first (newest) row when nothing valid is selected
 * yet, null when the list is empty.
 *
 * @param {Row[]} revisions
 * @param {number|string|null} selectedRev
 * @param {number} delta
 * @returns {number|string|null}
 */
export function moveSelection(revisions, selectedRev, delta) {
  if (revisions.length === 0) return null;
  const index = revisions.findIndex((rev) => rev.id === selectedRev);
  if (index === -1) return revisions[0].id;
  const next = Math.max(0, Math.min(index + delta, revisions.length - 1));
  return revisions[next].id;
}

/**
 * Returns the row id at the start (`'first'`, newest) or end (`'last'`,
 * oldest) of the list; null when the list is empty.
 *
 * @param {Row[]} revisions
 * @param {string} edge
 * @returns {number|string|null}
 */
export function edgeSelection(revisions, edge) {
  if (revisions.length === 0) return null;
  return edge === 'last' ? revisions[revisions.length - 1].id : revisions[0].id;
}
