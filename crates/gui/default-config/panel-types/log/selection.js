// log panel selection movement: revisions are listed in reverse
// chronological order, so delta +1 moves down the list (older).

/**
 * A revision, of which only the id matters here.
 * @typedef {{id: number}} Revision
 */

/**
 * Returns the revision id selected after moving by `delta` rows from
 * `selectedRev`, clamped to the list; the first (newest) revision when
 * nothing valid is selected yet, null when the log is empty.
 *
 * @param {Revision[]} revisions
 * @param {number|null} selectedRev
 * @param {number} delta
 * @returns {number|null}
 */
export function moveSelection(revisions, selectedRev, delta) {
  if (revisions.length === 0) return null;
  const index = revisions.findIndex((rev) => rev.id === selectedRev);
  if (index === -1) return revisions[0].id;
  const next = Math.max(0, Math.min(index + delta, revisions.length - 1));
  return revisions[next].id;
}

/**
 * Returns the revision id at the start (`'first'`, newest) or end
 * (`'last'`, oldest) of the list; null when the log is empty.
 *
 * @param {Revision[]} revisions
 * @param {string} edge
 * @returns {number|null}
 */
export function edgeSelection(revisions, edge) {
  if (revisions.length === 0) return null;
  return edge === 'last' ? revisions[revisions.length - 1].id : revisions[0].id;
}
