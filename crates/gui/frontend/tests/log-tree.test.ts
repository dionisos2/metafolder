// The log panel as a tree of labels (panel-types/log/tree.js, doc "log
// panel", doc "Log context summaries"): the units of work as the daemon summarises
// them, the watcher's flushes folded, a label's contents put in when it is gone
// into, and what each node is — the daemon's, an undo, undone.

import { describe, expect, test } from 'vitest';
import {
  buildTree,
  countOf,
  keyOf,
  marksOf,
  nodesAlong,
  pathTo,
  revertTarget,
  rowsAt,
  timeOf,
  withChildren,
} from '../../default-config/panel-types/log/tree.js';

type Op = {
  id: number;
  log_context_id: number;
  op_type: string;
  origin?: string | null;
  reverts_op_id?: number | null;
  timestamp?: number;
};

const op = (id: number, label: number, extra: Partial<Op> = {}): Op => ({
  id,
  log_context_id: label,
  op_type: 'set_field',
  origin: null,
  reverts_op_id: null,
  timestamp: id * 1000,
  ...extra,
});

/** A label as `GET /log/contexts` summarises it. */
const summary = (id: number, name: string, extra: Record<string, unknown> = {}) => ({
  id,
  name,
  description: null,
  parent: null,
  created: 0,
  operations: 1,
  last_op: id * 10,
  updated: id * 10000,
  watcher: false,
  children: 0,
  reverts: [],
  reverted_by: [],
  undone: null,
  ...extra,
});
type Summary = ReturnType<typeof summary>;
const flush = (id: number) => summary(id, 'watcher flush', { watcher: true });

const keys = (units: Summary[], showWatcher = true) =>
  rowsAt(buildTree(units as never), [], showWatcher).map((r) => r.key);

describe('buildTree', () => {
  test('the units of work are rows in the order listed, one label each', () => {
    expect(keys([summary(9, 'edit'), summary(1, 'run')])).toEqual(['label:9', 'label:1']);
  });

  test('consecutive flushes of the watcher fold into one row; one alone does not', () => {
    const units = [flush(5), summary(4, 'edit'), flush(3), flush(2)];
    expect(keys(units)).toEqual(['label:5', 'label:4', 'watcher:3']);
    const tree = buildTree(units);
    expect(rowsAt(tree, ['watcher:3'], true).map((r) => r.key)).toEqual(['label:3', 'label:2']);
    expect(keys(units, false)).toEqual(['label:4']);
    // Hiding the watcher's work filters the top level only: a fold gone into
    // still lists its flushes.
    expect(rowsAt(tree, ['watcher:3'], false).map((r) => r.key)).toEqual(['label:3', 'label:2']);
    expect(countOf(tree[2])).toBe(2);
  });
});

describe('withChildren', () => {
  test('a label lists what is nested in it beside its own operations, newest first', () => {
    const tree = buildTree([summary(1, 'run', { children: 1, operations: 2 })] as never);
    expect(rowsAt(tree, ['label:1'], true)).toEqual([]);
    withChildren(
      tree[0],
      [summary(2, 'answer', { parent: 1, last_op: 11 })] as never,
      [op(12, 1), op(10, 1)],
    );
    expect(rowsAt(tree, ['label:1'], true).map((r) => r.key)).toEqual(['op:12', 'label:2', 'op:10']);
  });

  test('an operation of a label undone whole is undone', () => {
    const tree = buildTree([summary(1, 'edit', { undone: 'fully' })] as never);
    withChildren(tree[0], [], [op(10, 1)]);
    const [row] = rowsAt(tree, ['label:1'], true);
    expect(marksOf(row.node).undone).toBe('fully');
  });
});

describe('the path gone into', () => {
  const tree = () => {
    const t = buildTree([summary(9, 'edit'), summary(1, 'run', { children: 1 })] as never);
    withChildren(t[1], [summary(2, 'answer', { parent: 1, children: 1 })] as never, []);
    const answer = (t[1] as unknown as { children: never[] }).children[0];
    withChildren(answer, [summary(3, 'command', { parent: 2 })] as never, []);
    const command = (answer as unknown as { children: never[] }).children[0];
    withChildren(command, [], [op(11, 3), op(10, 3)]);
    return t;
  };

  test('pathTo: the nodes to go into for a node to be listed, outermost first', () => {
    const t = tree();
    expect(pathTo(t, 'op:11')).toEqual(['label:1', 'label:2', 'label:3']);
    expect(pathTo(t, 'label:2')).toEqual(['label:1']);
    expect(pathTo(t, 'label:9')).toEqual([]);
    expect(pathTo(t, 'op:99')).toBeNull();
  });

  test('nodesAlong: the nodes of a path, cut where it no longer leads, or is not read yet', () => {
    const t = tree();
    expect(nodesAlong(t, ['label:1', 'label:2']).map(keyOf)).toEqual(['label:1', 'label:2']);
    expect(nodesAlong(t, ['label:1', 'label:7', 'label:3']).map(keyOf)).toEqual(['label:1']);
    // A label not gone into holds nothing to follow.
    expect(nodesAlong(t, ['label:9', 'label:4']).map(keyOf)).toEqual(['label:9']);
  });

  test('an operation is gone into last: it lists nothing, and nothing is below it', () => {
    const t = tree();
    const path = ['label:1', 'label:2', 'label:3', 'op:10'];
    expect(nodesAlong(t, path).map(keyOf)).toEqual(path);
    expect(nodesAlong(t, [...path, 'op:11']).map(keyOf)).toEqual(path);
    expect(rowsAt(t, path, true)).toEqual([]);
  });
});

describe('marksOf', () => {
  test('an undo names what it undid, and what it undid says it is undone', () => {
    const [undo, edit] = buildTree([
      summary(2, 'mf log undo 1', { reverts: [{ log_context: 1, whole: true }] }),
      summary(1, 'edit', { undone: 'fully', reverted_by: [{ log_context: 2, whole: true }] }),
    ] as never);
    expect(marksOf(undo).undoOf).toEqual([1]);
    expect(marksOf(edit).undone).toBe('fully');
    expect(marksOf(edit).undoneBy).toEqual([2]);
  });

  test('a redo stands the change back up: the daemon says so, the panel shows it', () => {
    const [edit] = buildTree([summary(1, 'edit', { undone: null })] as never);
    expect(marksOf(edit).undone).toBe(null);
  });

  test('the daemon’s work is marked by its summary, a label partly undone says so', () => {
    const [flushed, two] = buildTree([
      flush(1),
      summary(2, 'two edits', { undone: 'partly' }),
    ] as never);
    expect(marksOf(flushed).watcher).toBe(true);
    expect(marksOf(two).undone).toBe('partly');
  });

  test('an operation the watcher wrote, or an undo of one, is told by its origin and type', () => {
    const t = buildTree([summary(1, 'x')] as never);
    withChildren(t[0], [], [op(10, 1, { origin: 'watcher' }), op(11, 1, { op_type: 'file_moved' })]);
    const rows = rowsAt(t, ['label:1'], true);
    expect(rows.map((r) => marksOf(r.node).watcher)).toEqual([true, true]);
  });
});

describe('what a row says', () => {
  test('its count and the time it was last written in', () => {
    const [unit] = buildTree([summary(3, 'run', { operations: 12, updated: 5000 })] as never);
    expect(countOf(unit)).toBe(12);
    expect(timeOf(unit)).toBe(5000);
    const t = buildTree([summary(1, 'x')] as never);
    withChildren(t[0], [], [op(10, 1)]);
    const [row] = rowsAt(t, ['label:1'], true);
    expect(countOf(row.node)).toBeNull();
    expect(timeOf(row.node)).toBe(10000);
  });
});

describe('revertTarget', () => {
  test('a label is reverted whole, an operation alone, a fold not at all', () => {
    expect(revertTarget('label:3')).toEqual({ log_context_id: 3 });
    expect(revertTarget('op:7')).toEqual({ op_ids: [7] });
    expect(revertTarget('watcher:3')).toBe(null);
    expect(revertTarget(null)).toBe(null);
  });
});
