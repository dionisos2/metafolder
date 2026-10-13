// The log panel as a tree of labels (panel-types/log/tree.js, doc "log
// panel"): the operations of the window grouped by the labels they were written
// in, the watcher's flushes folded, and what each node is — the daemon's, an
// undo, undone.

import { describe, expect, test } from 'vitest';
import {
  buildTree,
  marksOf,
  revertTarget,
  rows,
} from '../../default-config/panel-types/log/tree.js';

type Op = {
  id: number;
  label_id: number;
  op_type: string;
  origin?: string | null;
  reverts_op_id?: number | null;
  timestamp?: number;
};

const op = (id: number, label: number, extra: Partial<Op> = {}): Op => ({
  id,
  label_id: label,
  op_type: 'set_field',
  origin: null,
  reverts_op_id: null,
  timestamp: id * 1000,
  ...extra,
});
const watcher = (id: number, label: number) =>
  op(id, label, { origin: 'watcher', op_type: 'create_metarecord' });
const label = (id: number, name: string, parent: number | null = null) => ({
  id,
  name,
  description: null,
  parent,
  created: 0,
});

/** The keys of the rows shown, `expanded` opened. */
type Label = ReturnType<typeof label>;
const keys = (ops: Op[], labels: Label[], expanded: string[] = [], showWatcher = true) =>
  rows(buildTree(ops, labels), new Set(expanded), showWatcher).map((r) => r.key);

describe('buildTree', () => {
  test('a script run is one row, opening onto its answers, then their commands', () => {
    const labels = [
      label(1, 'gui-tag-folder music'),
      label(2, 'answer /a: y', 1),
      label(3, 'mf tag add', 2),
      label(4, 'answer /b: n', 1),
      label(5, 'mf tag deny', 4),
      label(9, 'PUT …/fields/:name'),
    ];
    const ops = [op(10, 3), op(11, 3), op(12, 5), op(13, 9)];
    expect(keys(ops, labels)).toEqual(['label:9', 'label:1']);
    expect(keys(ops, labels, ['label:1'])).toEqual(['label:9', 'label:1', 'label:4', 'label:2']);
    expect(keys(ops, labels, ['label:1', 'label:2', 'label:3'])).toEqual([
      'label:9',
      'label:1',
      'label:4',
      'label:2',
      'label:3',
      'op:11',
      'op:10',
    ]);
    const run = rows(buildTree(ops, labels), new Set(), true)[1];
    expect(run.depth).toBe(0);
    expect(run.node.kind === 'label' && run.node.ops.map((o) => o.id)).toEqual([12, 11, 10]);
  });

  test("consecutive flushes of the watcher fold into one row; one alone does not", () => {
    const labels = [label(1, 'watcher'), label(2, 'watcher flush', 1), label(3, 'watcher flush', 1),
      label(4, 'edit'), label(5, 'watcher flush', 1)];
    const ops = [watcher(10, 2), watcher(11, 3), op(12, 4), watcher(13, 5)];
    expect(keys(ops, labels)).toEqual(['label:5', 'label:4', 'watcher:3']);
    expect(keys(ops, labels, ['watcher:3'])).toEqual(['label:5', 'label:4', 'watcher:3', 'label:3', 'label:2']);
    expect(keys(ops, labels, [], false)).toEqual(['label:4']);
  });

  test('a label with operations of its own lists them beside its children, newest first', () => {
    const labels = [label(1, 'run'), label(2, 'answer', 1)];
    const ops = [op(10, 1), op(11, 2), op(12, 1)];
    expect(keys(ops, labels, ['label:1'])).toEqual(['label:1', 'op:12', 'label:2', 'op:10']);
  });
});

describe('marksOf', () => {
  test('an undo names what it undid, and what it undid says it is undone', () => {
    const labels = [label(1, 'edit'), label(2, 'mf log undo 1')];
    const ops = [op(10, 1), op(11, 2, { reverts_op_id: 10 })];
    const [undo, edit] = buildTree(ops, labels);
    const marks = marksOf(ops);
    expect(marks(undo).undoOf).toEqual([1]);
    expect(marks(edit).undone).toBe('fully');
    expect(marks(edit).undoneBy).toEqual([2]);
  });

  test('a redo stands the change back up', () => {
    const labels = [label(1, 'edit'), label(2, 'undo'), label(3, 'redo')];
    const ops = [op(10, 1), op(11, 2, { reverts_op_id: 10 }), op(12, 3, { reverts_op_id: 11 })];
    const marks = marksOf(ops);
    const edit = buildTree(ops, labels).find((n) => n.kind === 'label' && n.id === 1);
    expect(edit && marks(edit).undone).toBe(null);
  });

  test('the daemon’s work is marked by its origin, a label partly undone says so', () => {
    const labels = [label(1, 'flush'), label(2, 'two edits'), label(3, 'undo')];
    const ops = [watcher(9, 1), op(10, 2), op(11, 2), op(12, 3, { reverts_op_id: 10 })];
    const marks = marksOf(ops);
    const tree = buildTree(ops, labels);
    const find = (id: number) => {
      const found = tree.find((n) => n.kind === 'label' && n.id === id);
      if (!found) throw new Error(`no label ${id}`);
      return found;
    };
    expect(marks(find(1)).watcher).toBe(true);
    expect(marks(find(2)).undone).toBe('partly');
  });
});

describe('revertTarget', () => {
  test('a label is reverted whole, an operation alone, a fold not at all', () => {
    expect(revertTarget('label:3')).toEqual({ label_id: 3 });
    expect(revertTarget('op:7')).toEqual({ op_ids: [7] });
    expect(revertTarget('watcher:3')).toBe(null);
    expect(revertTarget(null)).toBe(null);
  });
});
