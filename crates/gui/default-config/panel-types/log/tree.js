// log panel tree: the labels of the log, one level at a time, as the daemon
// summarises them (doc "log panel", doc "Log context summaries", doc "Labels and
// contexts").
//
// A row of the top level is a unit of work: for an operation of yours, its
// outermost label — a script's run, a command —; for one of the watcher, its
// flush. Going into a unit lists the labels nested in it and its own
// operations, and so on down to an operation, and into an operation for what it
// changed: the panel shows one level at a time, like the path-ref panel (doc
// "Opening a row"). Consecutive flushes of the watcher fold into one row, or
// they would drown the rest. Everything is newest first.
//
// No operation is read to list a label: a unit is its summary (how many
// operations it holds, who wrote it, what it undid and what undid it), and the
// operations and the labels nested in a unit are read when it is gone into —
// `withChildren` puts them in its node. Pure: the panel hands it what the
// daemon answered and renders what it builds.

/** Operation types the watcher records on the filesystem's behalf: the
 *  fallback when no origin says. */
const WATCHER_OP_TYPES = ['file_deleted', 'file_moved', 'file_modified'];

/**
 * One operation, as the daemon lists it.
 * @typedef {{id: number, label_id: number, op_type: string, origin?: string|null,
 *            reverts_op_id?: number|null, timestamp?: number,
 *            field_name?: string|null, entity_uuid?: string|null}} Op
 *
 * One label, as `GET /log/labels` summarises it.
 * @typedef {{id: number, name: string, description?: string|null,
 *            parent: number|null, created: number, updated?: number|null,
 *            operations: number, first_op?: number|null, last_op?: number|null,
 *            watcher?: boolean, children?: number, is_head?: boolean,
 *            reverts?: {label: number, whole: boolean}[],
 *            reverted_by?: {label: number, whole: boolean}[],
 *            undone?: 'fully'|'partly'|null}} Label
 *
 * A node of the tree. A label: its summary and, once it has been gone into,
 * what it holds (`children`: its labels and its own operations, newest first;
 * `null` before). An operation (`undone`: that of the label it is in, when
 * that is undone whole). A fold of the watcher's flushes.
 * @typedef {{kind: 'label', id: number, label: Label, children: Node[]|null,
 *            newest: number, watcher: boolean}
 *         | {kind: 'op', op: Op, newest: number, undone: 'fully'|null}
 *         | {kind: 'watcher', id: number, children: Node[], operations: number,
 *            newest: number}} Node
 *
 * A row on screen.
 * @typedef {{key: string, node: Node}} Row
 */

/** @param {Op} op */
export function isWatcherOp(op) {
  return (
    op.origin === 'watcher' ||
    (op.reverts_op_id == null && WATCHER_OP_TYPES.includes(op.op_type))
  );
}

/** The key a node is selected, gone into and found by.
 *  @param {Node} node */
export function keyOf(node) {
  if (node.kind === 'op') return `op:${node.op.id}`;
  return `${node.kind}:${node.id}`;
}

/** The node of a labelled summary, not gone into yet.
 *  @param {Label} label @returns {Extract<Node, {kind: 'label'}>} */
export function labelNode(label) {
  return {
    kind: 'label',
    id: label.id,
    label,
    children: null,
    newest: label.last_op ?? label.id,
    watcher: label.watcher === true,
  };
}

/**
 * The top level: the units of work, newest first as listed, the consecutive
 * flushes of the watcher folded into one row.
 * @param {Label[]} units
 * @returns {Node[]}
 */
export function buildTree(units) {
  const nodes = units.map(labelNode);
  /** @type {Node[]} */
  const out = [];
  for (let i = 0; i < nodes.length; i++) {
    let j = i;
    while (j + 1 < nodes.length && nodes[i].watcher && nodes[j + 1].watcher) j++;
    if (j === i) {
      out.push(nodes[i]);
      continue;
    }
    const run = nodes.slice(i, j + 1);
    out.push({
      kind: 'watcher',
      id: run[0].id,
      children: run,
      operations: run.reduce((n, u) => n + (u.kind === 'label' ? u.label.operations : 0), 0),
      newest: run[0].newest,
    });
    i = j;
  }
  return out;
}

/**
 * Puts in a label's node what it holds: the labels nested in it and its own
 * operations (`ops`, the newest of them), newest first.
 * @param {Node} node a label's @param {Label[]} nested @param {Op[]} ops
 * @returns {Node}
 */
export function withChildren(node, nested, ops) {
  if (node.kind !== 'label') return node;
  const under = node.label.undone === 'fully' ? 'fully' : null;
  /** @type {Node[]} */
  const children = [
    ...nested.map(labelNode),
    ...ops.map((op) => /** @type {Node} */ ({ kind: 'op', op, newest: op.id, undone: under })),
  ];
  children.sort((a, b) => b.newest - a.newest);
  node.children = children;
  return node;
}

/**
 * The nodes of `path` (keys, outermost first), each a child of the one before
 * — cut at the first that is not (a refresh or a prune took it away), and after
 * an operation: gone into, it shows what it changed and holds nothing.
 * @param {Node[]} tree @param {string[]} path
 * @returns {Node[]}
 */
export function nodesAlong(tree, path) {
  /** @type {Node[]} */
  const out = [];
  let level = tree;
  for (const key of path) {
    const next = level.find((n) => keyOf(n) === key);
    if (!next) break;
    out.push(next);
    if (next.kind === 'op' || next.children === null) break;
    level = next.children;
  }
  return out;
}

/**
 * The rows on screen: the children of the node `path` leads to, or the top
 * level; none in an operation. With the watcher's work hidden, its units and
 * folds are left out of the top level.
 * @param {Node[]} tree @param {string[]} path @param {boolean} showWatcher
 * @returns {Row[]}
 */
export function rowsAt(tree, path, showWatcher) {
  const along = nodesAlong(tree, path);
  const last = along[along.length - 1];
  if (last?.kind === 'op') return [];
  const level = last ? (last.children ?? []) : tree;
  return level
    .filter(
      (n) =>
        showWatcher ||
        along.length > 0 ||
        !(n.kind === 'watcher' || (n.kind === 'label' && n.watcher)),
    )
    .map((n) => ({ key: keyOf(n), node: n }));
}

/**
 * The path to go into for `key` to be listed: the keys of its ancestors,
 * outermost first; null when no node has that key.
 * @param {Node[]} tree @param {string} key
 * @returns {string[]|null}
 */
export function pathTo(tree, key) {
  /** @param {Node[]} nodes @param {string[]} above @returns {string[]|null} */
  const find = (nodes, above) => {
    for (const n of nodes) {
      if (keyOf(n) === key) return above;
      if (n.kind !== 'op' && n.children !== null) {
        const found = find(n.children, [...above, keyOf(n)]);
        if (found) return found;
      }
    }
    return null;
  };
  return find(tree, []);
}

/**
 * What a node is: the daemon's, an undo (of which labels), undone (fully or
 * partly, by which labels) — read off the summary of a label, which the daemon
 * worked out from the labels' marks (a redo stands the change back up), and an
 * operation takes it from the label it is in when that is undone whole.
 * @param {Node} node
 * @returns {{watcher: boolean, undoOf: number[], undone: 'fully'|'partly'|null,
 *           undoneBy: number[]}}
 */
export function marksOf(node) {
  if (node.kind === 'op') {
    return { watcher: isWatcherOp(node.op), undoOf: [], undone: node.undone, undoneBy: [] };
  }
  if (node.kind === 'watcher') return { watcher: true, undoOf: [], undone: null, undoneBy: [] };
  const l = node.label;
  return {
    watcher: l.watcher === true,
    undoOf: (l.reverts ?? []).map((r) => r.label),
    undone: l.undone ?? null,
    undoneBy: (l.reverted_by ?? []).map((r) => r.label),
  };
}

/**
 * How many operations a row says: a label's own, a fold's flushes'; none for an
 * operation.
 * @param {Node} node
 */
export function countOf(node) {
  if (node.kind === 'op') return null;
  return node.kind === 'watcher' ? node.operations : node.label.operations;
}

/**
 * When a row was last written in, in milliseconds (0 when it says nothing).
 * @param {Node} node
 * @returns {number}
 */
export function timeOf(node) {
  if (node.kind === 'op') return node.op.timestamp ?? 0;
  if (node.kind === 'watcher') return Math.max(0, ...node.children.map(timeOf));
  return node.label.updated ?? node.label.created ?? 0;
}

/**
 * The `POST /revert` target of a selected row: a label whole (its children
 * included), one operation alone; nothing for a fold of the watcher's flushes.
 * @param {string|null} key
 * @returns {{label_id: number}|{op_ids: number[]}|null}
 */
export function revertTarget(key) {
  if (!key) return null;
  const [kind, raw] = key.split(':');
  const id = Number(raw);
  if (kind === 'label') return { label_id: id };
  if (kind === 'op') return { op_ids: [id] };
  return null;
}
