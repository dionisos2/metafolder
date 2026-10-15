// log panel tree: the operations of the fetched window, grouped by the labels
// they were written in (doc "log panel", doc "Labels and contexts").
//
// A row of the top level is a unit of work: for an operation of yours, its
// outermost label — a script's run, a command —; for one of the watcher, its
// flush. Going into a unit lists the labels nested in it, and so on down to the
// operations, and into an operation for what it changed: the panel shows one
// level at a time, like the treeref panel (doc "Opening a row").
// Consecutive flushes of the watcher fold into one row, or they would drown
// the rest. Everything is newest first, by the newest operation under it.
//
// Pure: the panel hands it what `GET /log` answered and renders what it builds.

/** Operation types the watcher records on the filesystem's behalf: the
 *  fallback when no origin says. */
const WATCHER_OP_TYPES = ['file_deleted', 'file_moved', 'file_modified'];

/**
 * One operation, as `GET /log` returns it.
 * @typedef {{id: number, label_id: number, op_type: string, origin?: string|null,
 *            reverts_op_id?: number|null, timestamp?: number,
 *            field_name?: string|null, entity_uuid?: string|null}} Op
 *
 * One label, as `GET /log` lists it.
 * @typedef {{id: number, name: string, description?: string|null,
 *            parent: number|null, created: number}} Label
 *
 * A node of the tree. A label: the label, every operation under it in the
 * window (`ops`, newest first) and its children (labels and its own
 * operations). An operation. A fold of the watcher's flushes.
 * @typedef {{kind: 'label', id: number, label: Label, ops: Op[], children: Node[],
 *            newest: number, watcher: boolean}
 *         | {kind: 'op', op: Op, newest: number}
 *         | {kind: 'watcher', id: number, children: Node[], ops: Op[], newest: number}} Node
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

/**
 * The tree of the window, its top level newest first.
 * @param {Op[]} ops @param {Label[]} labels
 * @returns {Node[]}
 */
export function buildTree(ops, labels) {
  /** @type {Map<number, Label>} */
  const byId = new Map(labels.map((l) => [l.id, l]));
  /** The labels from `id` up, innermost first, as far as the window names them.
   *  @param {number} id */
  const chain = (id) => {
    const out = [id];
    let next = byId.get(id)?.parent ?? null;
    while (next !== null && byId.has(next) && !out.includes(next)) {
      out.push(next);
      next = byId.get(next)?.parent ?? null;
    }
    return out;
  };

  /** @type {Map<number, Node & {kind: 'label'}>} */
  const nodes = new Map();
  /** @param {number} id */
  const node = (id) => {
    let n = nodes.get(id);
    if (!n) {
      const label = byId.get(id) ?? { id, name: '', description: null, parent: null, created: 0 };
      n = { kind: 'label', id, label, ops: [], children: [], newest: 0, watcher: true };
      nodes.set(id, n);
    }
    return n;
  };

  /** @type {Set<number>} the ids of the top-level units */
  const tops = new Set();
  for (const op of ops) {
    // The watcher's flush is its own unit, under a root no one opens.
    const path = isWatcherOp(op) ? [op.label_id] : chain(op.label_id);
    const top = path[path.length - 1];
    tops.add(top);
    for (let i = 0; i < path.length; i++) {
      const n = node(path[i]);
      n.ops.push(op);
      n.newest = Math.max(n.newest, op.id);
      n.watcher = n.watcher && isWatcherOp(op);
      /** @type {Node} */
      const child = i === 0 ? { kind: 'op', op, newest: op.id } : node(path[i - 1]);
      if (i === 0 || !n.children.includes(child)) n.children.push(child);
    }
  }
  const newestFirst = (/** @type {Node} */ a, /** @type {Node} */ b) => b.newest - a.newest;
  for (const n of nodes.values()) {
    n.children.sort(newestFirst);
    n.ops.sort((a, b) => b.id - a.id);
  }

  const units = [...tops].map((id) => node(id)).sort(newestFirst);
  /** @type {Node[]} */
  const out = [];
  for (let i = 0; i < units.length; i++) {
    let j = i;
    while (j + 1 < units.length && units[i].watcher && units[j + 1].watcher) j++;
    if (j === i) {
      out.push(units[i]);
      continue;
    }
    const run = units.slice(i, j + 1);
    out.push({
      kind: 'watcher',
      id: run[0].id,
      children: run,
      ops: run.flatMap((u) => u.ops),
      newest: run[0].newest,
    });
    i = j;
  }
  return out;
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
    if (next.kind === 'op') break;
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
  const level = last ? last.children : tree;
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
      if (n.kind !== 'op') {
        const found = find(n.children, [...above, keyOf(n)]);
        if (found) return found;
      }
    }
    return null;
  };
  return find(tree, []);
}

/**
 * What each node is, from the operations of the window: the daemon's, an undo
 * (of which labels), undone (fully or partly, by which labels). Liveness is
 * resolved along the chain — a redo stands the change back up — and a revert
 * is always newer than what it reverts, so the window holds every revert of
 * what it holds.
 * @param {Op[]} ops
 * @returns {(node: Node) => {watcher: boolean, undoOf: number[],
 *           undone: 'fully'|'partly'|null, undoneBy: number[]}}
 */
export function marksOf(ops) {
  /** @type {Map<number, Op>} */
  const byId = new Map(ops.map((o) => [o.id, o]));
  /** @type {Map<number, Op[]>} */
  const reverters = new Map();
  for (const o of ops) {
    if (o.reverts_op_id != null) {
      reverters.set(o.reverts_op_id, [...(reverters.get(o.reverts_op_id) ?? []), o]);
    }
  }
  /** @type {Set<number>} */
  const undone = new Set();
  for (const o of [...ops].sort((a, b) => b.id - a.id)) {
    if ((reverters.get(o.id) ?? []).some((r) => !undone.has(r.id))) undone.add(o.id);
  }
  return (node) => {
    const mine = node.kind === 'op' ? [node.op] : node.ops;
    const undoOf = new Set();
    const undoneBy = new Set();
    for (const o of mine) {
      const target = o.reverts_op_id == null ? undefined : byId.get(o.reverts_op_id);
      if (target) undoOf.add(target.label_id);
      for (const r of reverters.get(o.id) ?? []) if (!undone.has(r.id)) undoneBy.add(r.label_id);
    }
    const count = mine.filter((o) => undone.has(o.id)).length;
    return {
      watcher: mine.length > 0 && mine.every(isWatcherOp),
      undoOf: [...undoOf],
      undone: count === 0 ? null : count === mine.length ? 'fully' : 'partly',
      undoneBy: [...undoneBy],
    };
  };
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
