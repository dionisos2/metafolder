// Log panel annotations (panel-types/log/annotate.js): telling apart the
// revisions a user wrote, the ones the daemon wrote for the filesystem, the
// ones that are themselves an undo, and the ones already undone — which is what
// makes a revert something you can aim at rather than guess.

import { describe, expect, test } from 'vitest';
import { annotate, revertTarget } from '../../default-config/panel-types/log/annotate.js';

const op = (
  id: number,
  revId: number,
  type = 'set_field',
  reverts: number | null = null,
) => ({ id, rev_id: revId, op_type: type, reverts_op_id: reverts });

/** The marks of one revision, which the tests always ask for by a real id. */
const marksOf = (marks: Map<number, any>, id: number) => {
  const mark = marks.get(id);
  expect(mark, `no marks for revision ${id}`).toBeDefined();
  return mark;
};

describe('annotate', () => {
  test("a revision the daemon wrote for the filesystem is marked by its origin", () => {
    const revs = [{ id: 2, origin: 'watcher' }, { id: 1, origin: null }];
    const ops = [op(2, 2, 'create_metarecord'), op(1, 1)];
    const marks = annotate(revs, ops);
    expect(marksOf(marks, 2).watcher).toBe(true);
    expect(marksOf(marks, 1).watcher).toBe(false);
  });

  test('without an origin (an older database) the operation types are the fallback', () => {
    const revs = [{ id: 2, origin: null }, { id: 1, origin: null }];
    const ops = [op(2, 2, 'file_moved'), op(1, 1)];
    const marks = annotate(revs, ops);
    expect(marksOf(marks, 2).watcher).toBe(true);
    expect(marksOf(marks, 1).watcher).toBe(false);
  });

  test('a revert names what it undid, and its target says who undid it', () => {
    const revs = [{ id: 2, origin: null }, { id: 1, origin: null }];
    const ops = [op(2, 2, 'set_field', 1), op(1, 1)];
    const marks = annotate(revs, ops);
    expect(marksOf(marks, 2).reverts).toEqual([1]);
    expect(marksOf(marks, 2).undoneBy).toBe(null);
    expect(marksOf(marks, 1).undoneBy).toBe(2);
    expect(marksOf(marks, 1).fullyUndone).toBe(true);
  });

  test('a revision only partly undone says so', () => {
    const revs = [{ id: 2, origin: null }, { id: 1, origin: null }];
    const ops = [op(3, 2, 'set_field', 1), op(1, 1), op(2, 1)];
    const marks = annotate(revs, ops);
    expect(marksOf(marks, 1).undoneBy).toBe(2);
    expect(marksOf(marks, 1).fullyUndone).toBe(false);
    expect(marksOf(marks, 1).undoneOps).toEqual([1]);
  });

  test('a revert of an operation outside the fetched window is still a revert', () => {
    const revs = [{ id: 2, origin: null }];
    const ops = [op(2, 2, 'set_field', 999)];
    const marks = annotate(revs, ops);
    expect(marksOf(marks, 2).reverts).toEqual([]);
    expect(marksOf(marks, 2).isRevert).toBe(true);
  });

  test('an empty revision is nobody’s and nothing’s', () => {
    const marks = annotate([{ id: 1, origin: null }], []);
    expect(marksOf(marks, 1)).toEqual({
      watcher: false,
      isRevert: false,
      reverts: [],
      undoneBy: null,
      undoneOps: [],
      fullyUndone: false,
    });
  });
});

describe('revertTarget', () => {
  test('an operation selection targets that operation alone', () => {
    expect(revertTarget({ kind: 'op', id: 7, revId: 3 })).toEqual({ op_ids: [7] });
  });

  test('a revision selection targets the whole revision', () => {
    expect(revertTarget({ kind: 'rev', id: 3 })).toEqual({ rev_id: 3 });
  });

  test('nothing selected targets nothing', () => {
    expect(revertTarget(null)).toBe(null);
  });
});
