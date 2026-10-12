// Hiding the watcher's revisions in the log panel (panel-types/log): the list
// keeps the user's own history and HEAD.

import { describe, expect, test } from 'vitest';
import { shownRevisions } from '../../default-config/panel-types/log/annotate.js';

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
