// log panel selection movement: revisions are listed in reverse
// chronological order, so delta +1 moves down the list (older).

/**
 * Returns the revision id selected after moving by `delta` rows from
 * `selectedRev`, clamped to the list; the first (newest) revision when
 * nothing valid is selected yet, null when the log is empty.
 */
export function moveSelection(revisions, selectedRev, delta) {
  if (revisions.length === 0) return null;
  const index = revisions.findIndex((rev) => rev.id === selectedRev);
  if (index === -1) return revisions[0].id;
  const next = Math.max(0, Math.min(index + delta, revisions.length - 1));
  return revisions[next].id;
}
