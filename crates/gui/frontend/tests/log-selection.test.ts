// Log panel selection movement (panel-types/log/selection.js): arrow-key
// navigation over the reverse-chronological revision list.

import { describe, expect, test } from 'vitest';
// @ts-expect-error plain-JS module shared with the log panel type
import { moveSelection, edgeSelection } from '../../panel-types/log/selection.js';

const revisions = [{ id: 9 }, { id: 7 }, { id: 3 }]; // newest first

describe('moveSelection', () => {
  test('moves down (+1) and up (-1) through the listed order', () => {
    expect(moveSelection(revisions, 9, 1)).toBe(7);
    expect(moveSelection(revisions, 7, 1)).toBe(3);
    expect(moveSelection(revisions, 7, -1)).toBe(9);
  });

  test('clamps at both ends', () => {
    expect(moveSelection(revisions, 9, -1)).toBe(9);
    expect(moveSelection(revisions, 3, 1)).toBe(3);
  });

  test('with no selection yet, selects the first (newest) revision', () => {
    expect(moveSelection(revisions, null, 1)).toBe(9);
    expect(moveSelection(revisions, null, -1)).toBe(9);
  });

  test('a stale selection (pruned revision) restarts at the top', () => {
    expect(moveSelection(revisions, 42, 1)).toBe(9);
  });

  test('an empty log has nothing to select', () => {
    expect(moveSelection([], null, 1)).toBe(null);
  });
});

describe('edgeSelection', () => {
  test('first is the newest, last is the oldest', () => {
    expect(edgeSelection(revisions, 'first')).toBe(9);
    expect(edgeSelection(revisions, 'last')).toBe(3);
  });

  test('an empty log has no edge', () => {
    expect(edgeSelection([], 'first')).toBe(null);
    expect(edgeSelection([], 'last')).toBe(null);
  });
});
