// Back navigation for the metarecord-detail panel: the trail of records that
// were shown before the current one.
//
// The panel does not own its selection — `selected_metarecord` is a workspace
// variable any panel may move (a ref click here, a row in metarecord-list, a
// script). So the trail is fed from the one place that sees every move, the
// panel's `selected_metarecord` listener, and `back()` moves the same variable
// again. That round trip comes back through the listener, which is why the
// step a `back()` produces must not itself be recorded — otherwise the trail
// would flip between the last two records forever.

/** The identity a selection carries (`{uuid, repo}`), or null for none.
 *  @typedef {{uuid: string, repo: string}|null} Selection */

/** @param {Selection} a @param {Selection} b */
function same(a, b) {
  if (a === null || b === null) return a === b;
  return a.uuid === b.uuid && a.repo === b.repo;
}

/**
 * @param {{limit?: number}} [options] how many steps to keep (oldest dropped)
 */
export function createNavHistory({ limit = 50 } = {}) {
  /** @type {Selection[]} the records left behind, oldest first */
  const trail = [];
  // Set by `back()`, cleared by the `record()` its own move triggers.
  let returning = false;

  return {
    /** Records a move of the selection from `previous` to `next`.
     *  @param {Selection} previous @param {Selection} next */
    record(previous, next) {
      if (returning) {
        returning = false;
        return;
      }
      if (same(previous, next)) return; // a re-selection, not a move
      if (previous === null) return; // nothing was shown: no step to come back to
      trail.push(previous);
      if (trail.length > limit) trail.shift();
    },

    /** The record to go back to, or null when the trail is empty. The caller
     *  is expected to actually move the selection there.
     *  @returns {Selection} */
    back() {
      if (trail.length === 0) return null;
      returning = true;
      return trail.pop() ?? null;
    },

    /** Forget a `back()` whose move never happened (the panel refused the
     *  selection change), so the next real move is recorded normally. */
    cancel() {
      returning = false;
    },

    canGoBack: () => trail.length > 0,
    /** How many steps are behind us — for a status message. */
    depth: () => trail.length,
  };
}
