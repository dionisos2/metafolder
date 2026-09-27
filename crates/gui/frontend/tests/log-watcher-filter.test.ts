// Hiding the watcher's revisions in the log panel (panel-types/log): the list
// keeps the user's own history and HEAD, and the graph reattaches what is left
// across the hidden revisions so a branch still draws as one line.

import { describe, expect, test } from 'vitest';
import { shownRevisions } from '../../default-config/panel-types/log/annotate.js';
import { collapseParents } from '../../default-config/panel-types/log/graph.js';

const rev = (id: number, isHead = false) => ({ id, isHead });
const marks = (watcherIds: number[], ids: number[]) =>
  new Map(ids.map((id) => [id, { watcher: watcherIds.includes(id) }]));

describe('shownRevisions', () => {
  const revs = [rev(4), rev(3), rev(2), rev(1)];

  test('shows everything when watcher revisions are shown', () => {
    expect(shownRevisions(revs, marks([2, 3], [1, 2, 3, 4]), true).map((r) => r.id)).toEqual([
      4, 3, 2, 1,
    ]);
  });

  test('drops the watcher revisions when hidden', () => {
    expect(shownRevisions(revs, marks([2, 3], [1, 2, 3, 4]), false).map((r) => r.id)).toEqual([
      4, 1,
    ]);
  });

  test('never hides the revision HEAD stands on', () => {
    const withHead = [rev(2, true), rev(1)];
    expect(shownRevisions(withHead, marks([2], [1, 2]), false).map((r) => r.id)).toEqual([2, 1]);
  });
});

describe('collapseParents', () => {
  test('a shown revision is reattached to its nearest shown ancestor', () => {
    // 4 -> 3 -> 2 -> 1, with 3 and 2 hidden.
    const parents = new Map<number, number | null>([
      [4, 3],
      [3, 2],
      [2, 1],
      [1, null],
    ]);
    const collapsed = collapseParents(parents, new Set([4, 1]));
    expect(collapsed.get(4)).toBe(1);
    expect(collapsed.get(1)).toBe(null);
    expect(collapsed.has(3)).toBe(false);
  });

  test('an ancestry that is entirely hidden ends in null', () => {
    const parents = new Map<number, number | null>([
      [2, 1],
      [1, null],
    ]);
    expect(collapseParents(parents, new Set([2])).get(2)).toBe(null);
  });

  test('a parent outside the fetched window stays null rather than looping', () => {
    const parents = new Map<number, number | null>([[5, 99]]);
    expect(collapseParents(parents, new Set([5])).get(5)).toBe(null);
  });
});
