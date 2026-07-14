// @ts-nocheck — not typed yet; the JS is being converted file by file.
// Lane-based ASCII graph layout for the history forest. Mirrors the CLI's
// `graph_layout` (crates/cli/src/log.rs): the active line stays in the leftmost
// column, divergent branches open a column to the right and converge back with
// a `/` connector at their common parent. Pure and unit-tested.

/** Map of revId → parent revId (or null) from the flat operations list. */
export function revisionParents(operations) {
  const opRev = new Map(); // op id → rev id
  for (const o of operations) opRev.set(o.id, o.rev_id);

  const byRev = new Map(); // rev id → its ops
  for (const o of operations) {
    if (!byRev.has(o.rev_id)) byRev.set(o.rev_id, []);
    byRev.get(o.rev_id).push(o);
  }

  // A revision's parent is the revision of the parent of its root op — the one
  // op parented in another revision (or none, for a root revision).
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
 */
export function graphLayout(revs) {
  const lanes = []; // each entry: rev id the lane is waiting to draw, or null
  const out = [];
  for (const { id: rev, parent } of revs) {
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

function trimEnd(chars) {
  return chars.join('').replace(/\s+$/, '');
}
