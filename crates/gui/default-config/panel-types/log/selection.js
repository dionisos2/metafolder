// log panel selection movement: the rows are listed newest first, so delta
// +1 moves down the list (older).
//
// The rows are whatever the panel says they are: the units of work, with the
// labels and operations of the opened ones inlined below them. The caller keys
// the rows (`label:12`, `op:34`) so the kinds cannot collide; this module only
// walks the list it is handed.

/**
 * A selectable row, of which only the id matters here.
 * @typedef {{id: number|string}} Row
 */

/**
 * Returns the row id selected after moving by `delta` rows from `selectedId`,
 * clamped to the list; the first (newest) row when nothing valid is selected
 * yet, null when the list is empty.
 *
 * @param {Row[]} rows
 * @param {number|string|null} selectedId
 * @param {number} delta
 * @returns {number|string|null}
 */
export function moveSelection(rows, selectedId, delta) {
  if (rows.length === 0) return null;
  const index = rows.findIndex((row) => row.id === selectedId);
  if (index === -1) return rows[0].id;
  const next = Math.max(0, Math.min(index + delta, rows.length - 1));
  return rows[next].id;
}

/**
 * Returns the row id at the start (`'first'`, newest) or end (`'last'`,
 * oldest) of the list; null when the list is empty.
 *
 * @param {Row[]} rows
 * @param {string} edge
 * @returns {number|string|null}
 */
export function edgeSelection(rows, edge) {
  if (rows.length === 0) return null;
  return edge === 'last' ? rows[rows.length - 1].id : rows[0].id;
}
