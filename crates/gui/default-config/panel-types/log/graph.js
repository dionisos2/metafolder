// Lane-based ASCII graph layout for the history forest. Mirrors the CLI's
// `graph_layout` (crates/cli/src/log.rs): the active line stays in the leftmost
// column, divergent branches open a column to the right and converge back with
// a `/` connector at their common parent. Pure and unit-tested.

/**
 * One row of the daemon's flat operations list, of which the graph needs only
 * the chaining.
 * @typedef {{id: number, rev_id: number, parent_id?: number|null}} Operation
 *
 * Map of revId → parent revId (or null) from the flat operations list.
 * @param {Operation[]} operations
 * @returns {Map<number, number|null>}
 */
export function revisionParents(operations) {
  /** @type {Map<number, number>} op id → rev id */
  const opRev = new Map();
  for (const o of operations) opRev.set(o.id, o.rev_id);

  /** @type {Map<number, Operation[]>} rev id → its ops */
  const byRev = new Map();
  for (const o of operations) {
    const ops = byRev.get(o.rev_id);
    if (ops) ops.push(o);
    else byRev.set(o.rev_id, [o]);
  }

  // A revision's parent is the revision of the parent of its root op — the one
  // op parented in another revision (or none, for a root revision).
  /** @type {Map<number, number|null>} */
  const parents = new Map();
  for (const [rev, ops] of byRev) {
    let parentRev = null;
    for (const o of ops) {
      const p = o.parent_id ?? null;
      if (p === null) break; // root revision
      const pr = opRev.get(p) ?? null;
      if (pr !== rev) {
        parentRev = pr;
        break;
      }
    }
    parents.set(rev, parentRev);
  }
  return parents;
}

/**
 * Lays out revisions into graph lines. `revs` lists revisions most-recent first
 * as `{ id, parent }` (parent `null` for a root or when the parent is outside
 * the shown window). Returns lines `{ type: 'node', gutter, revId }` or
 * `{ type: 'connector', gutter }`, top (newest) to bottom.
 *
 * @param {{id: number, parent: number|null}[]} revs
 * @returns {({type: 'node', gutter: string, revId: number}
 *           |{type: 'connector', gutter: string})[]}
 */
export function graphLayout(revs) {
  /** @type {(number|null)[]} each entry: rev id the lane is waiting to draw, or null */
  const lanes = [];
  /** @type {({type: 'node', gutter: string, revId: number}
   *          |{type: 'connector', gutter: string})[]} */
  const out = [];
  for (const { id: rev, parent } of revs) {
    /** @type {number[]} */
    const hits = [];
    lanes.forEach((l, i) => {
      if (l === rev) hits.push(i);
    });

    let col;
    if (hits.length > 0) {
      col = hits[0];
    } else {
      // A tip with no waiting lane: reuse the leftmost free column.
      const free = lanes.indexOf(null);
      if (free >= 0) {
        lanes[free] = rev;
        col = free;
      } else {
        lanes.push(rev);
        col = lanes.length - 1;
      }
    }
    const extra = hits.filter((i) => i !== col);

    // Connector row above the node: extra child lanes slope into `col`.
    if (extra.length > 0) {
      const conn = new Array(lanes.length * 2).fill(' ');
      lanes.forEach((l, i) => {
        if (l !== null && !extra.includes(i)) conn[2 * i] = '|';
      });
      for (const e of extra) {
        if (e > col) conn[2 * e - 1] = '/';
        else conn[2 * e + 1] = '\\';
      }
      out.push({ type: 'connector', gutter: trimEnd(conn) });
      for (const e of extra) lanes[e] = null;
    }

    // Node row.
    const row = new Array(lanes.length * 2).fill(' ');
    lanes.forEach((l, i) => {
      row[2 * i] = i === col ? '*' : l !== null ? '|' : ' ';
    });
    out.push({ type: 'node', gutter: trimEnd(row), revId: rev });

    lanes[col] = parent;
  }
  return out;
}

/** @param {string[]} chars */
function trimEnd(chars) {
  return chars.join('').replace(/\s+$/, '');
}
