// The find bar's driver (spec-gui "Find in panel"): a browser-style Ctrl-F
// over the focused panel's rendered text. The matching itself is the pure,
// panel-agnostic `/__find.js` shim; this module holds the shell state (needle,
// match list, current index) and paints/scrolls.
//
// Every operation re-runs the search against the panel *as it is now* rather
// than reusing the previous match list: panels re-render under the bar (a
// query re-runs, a page loads), and a stale Range would then highlight text
// that is no longer there. Recomputing costs a DOM walk of one panel, which is
// far cheaper than the render that invalidated it.

import { createHighlighter, scrollRangeIntoView, search, toRange } from '../../../panel-shim/find.js';
import { focusedPanelRoot } from './panels/roots';
import { store } from './store.svelte';

const highlighter = createHighlighter();

/** The matches of the current needle, in document order. */
let ranges: Range[] = [];

/** Where the next/previous step lands, wrapping at both ends. `current` is -1
 *  when nothing is selected yet, so a first step selects an end match; an empty
 *  match list has no index at all. Pure, so it is unit tested. */
export function stepIndex(current: number, delta: number, count: number): number {
  if (count === 0) return -1;
  if (current < 0) return delta >= 0 ? 0 : count - 1;
  return (((current + delta) % count) + count) % count;
}

/** The Range of the current match, or null. */
export function currentRange(): Range | null {
  return ranges[store.ui.find.index] ?? null;
}

function paint() {
  highlighter.show(ranges, store.ui.find.index);
  const range = currentRange();
  if (range) scrollRangeIntoView(range);
}

/** Re-runs `needle` against the focused panel, keeping the current match
 *  position when it still exists (so re-running does not jump back to the
 *  top). */
export function runFind(needle: string) {
  const find = store.ui.find;
  find.needle = needle;
  const root = focusedPanelRoot();
  ranges = root === null || needle === '' ? [] : search(root, needle).map(toRange);
  find.count = ranges.length;
  find.index = ranges.length === 0 ? -1 : Math.min(Math.max(find.index, 0), ranges.length - 1);
  paint();
}

/** Moves to the next (`delta` 1) or previous (-1) match, wrapping. */
export function stepFind(delta: number) {
  const find = store.ui.find;
  runFind(find.needle);
  find.index = stepIndex(find.index, delta, ranges.length);
  paint();
}

/** Opens the bar (or re-focuses its input when it is already open) and runs
 *  `needle` — the last one when it is omitted, so Ctrl-F twice in a row keeps
 *  the search. An explicit needle is what `find:in-panel <text>` (and a script
 *  driving it through the GUI API) searches for. */
export function openFind(needle?: string) {
  const find = store.ui.find;
  find.open = true;
  find.focusTick += 1;
  if (needle !== undefined && needle !== find.needle) find.index = -1;
  runFind(needle ?? find.needle);
}

/** Closes the bar and unpaints. The needle is kept for the next Ctrl-F. */
export function closeFind() {
  const find = store.ui.find;
  find.open = false;
  find.count = 0;
  find.index = -1;
  ranges = [];
  highlighter.clear();
}
