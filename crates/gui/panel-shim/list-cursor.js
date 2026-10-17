// The cursor of a list of items (panel-shim/list-cursor.js, served as
// /__list-cursor.js): what every panel listing rows does when the cursor moves
// (doc "Moving the cursor").
//
// Held-arrow navigation must stay cheaper than the key-repeat rate, or key
// events accumulate and keep replaying after the key is released. Two things
// were slower than the repeat in the panels that had the bug:
//   - rebuilding every row to move one highlight (the log rebuilt up to a
//     thousand table rows per key) — here moving only retargets a class on the
//     rows already in the DOM, which must be index-aligned with the panel's
//     list;
//   - publishing the selection (two workspace.set IPC round-trips fanning out
//     to the other panels) once per key — here it is coalesced by
//     `latestOnly`: one in flight, one trailing with the final position.
//
// Headless about the list itself: the panel keeps its cursor index and its
// rows, and calls `move` instead of re-rendering.

import { latestOnly } from '/__coalesce.js';

/**
 * @param {object} spec
 * @param {() => Iterable<Element>} spec.containers the elements whose children
 *   are the rows, index-aligned with the panel's list (several when one list
 *   is shown in several layouts, e.g. a table and a grid)
 * @param {string} [spec.className] the class marking the cursor row
 * @param {(index: number) => unknown} [spec.publish] the selection's side
 *   effect, coalesced (only the final position of a burst matters)
 */
export function createListCursor({ containers, className = 'cursor', publish }) {
  const propagate = publish ? latestOnly(publish) : null;

  /** Moves the class onto the row at `index` (none past the rows), without
   *  touching the rows themselves. @param {number} index */
  function highlight(index) {
    for (const container of containers()) {
      for (const stale of container.querySelectorAll(`:scope > .${className}`)) {
        stale.classList.remove(className);
      }
      container.children[index]?.classList.add(className);
    }
  }

  /** Scrolls the cursor row into view, as little as needed. */
  function reveal() {
    for (const container of containers()) {
      container.querySelector(`:scope > .${className}`)?.scrollIntoView?.({ block: 'nearest' });
    }
  }

  /** Highlights, reveals and publishes `index`; resolves once the publication
   *  has caught up with this move. @param {number} index */
  async function move(index) {
    highlight(index);
    reveal();
    if (propagate) await propagate(index);
  }

  /** Publishes `index` without moving the highlight (a selection to catch
   *  up), in the same coalescing as the moves. @param {number} index */
  async function publishOnly(index) {
    if (propagate) await propagate(index);
  }

  return { highlight, reveal, move, publish: publishOnly };
}
