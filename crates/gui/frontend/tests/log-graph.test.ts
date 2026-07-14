import { describe, expect, test } from 'vitest';

import { graphLayout, revisionParents } from '../../default-config/panel-types/log/graph.js';

const gutters = (revs: { id: number; parent: number | null }[]) =>
  graphLayout(revs).map((l: { gutter: string }) => l.gutter);

describe('graphLayout', () => {
  test('linear history is a single column', () => {
    const revs = [
      { id: 3, parent: 2 },
      { id: 2, parent: 1 },
      { id: 1, parent: null },
    ];
    expect(gutters(revs)).toEqual(['*', '*', '*']);
  });

  test('a branch opens a column and converges with a slash', () => {
    // rev3 (the active line) and rev2 are both children of rev1.
    const revs = [
      { id: 3, parent: 1 },
      { id: 2, parent: 1 },
      { id: 1, parent: null },
    ];
    expect(gutters(revs)).toEqual(['*', '| *', '|/', '*']);
  });
});

describe('revisionParents', () => {
  test('resolves each revision to the revision of its root op parent', () => {
    // rev1: op1 (root). rev2: op2 (parent op1). rev3: op3 (parent op1) — a branch.
    const operations = [
      { id: 1, parent_id: null, rev_id: 1 },
      { id: 2, parent_id: 1, rev_id: 2 },
      { id: 3, parent_id: 1, rev_id: 3 },
    ];
    const parents = revisionParents(operations);
    expect(parents.get(1)).toBe(null);
    expect(parents.get(2)).toBe(1);
    expect(parents.get(3)).toBe(1);
  });
});
