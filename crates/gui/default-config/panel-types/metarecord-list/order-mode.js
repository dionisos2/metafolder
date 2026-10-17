// Ordering the list by an int field (doc "Ordering a list"): the numbering
// `metarecord-list:order on` writes, the writes of `order move`, and the local
// reorder that keeps the loaded rows in step without reading them again.
//
// A row here is `{uuid, value}`: a metarecord and its position in the field
// (`null` when it has none).

import { MATCH_ALL } from '/__value-widget.js';

/** Above this many positions to write, `order on` asks first. */
export const CONFIRM_ABOVE = 50;
/** Positions per `query/fields/batch`: a batch stays under the daemon's 2 MiB
 *  body limit (the same bound as `metafolder_core::order`). */
export const BATCH_OPS = 2000;
/** Page size of the read that gathers every match. */
const READ_PAGE = 500;

/** The directions `order move` takes. */
export const MOVES = ['up', 'down', 'top', 'bottom'];

/**
 * A metarecord's position in `field`: its smallest int value — the one the
 * daemon sorts it on ascending — or null without one.
 * @param {Metafolder.Metarecord} metarecord @param {string} field
 * @returns {number|null}
 */
export function positionOf(metarecord, field) {
  let best = null;
  for (const f of metarecord.fields ?? []) {
    if (f.name !== field || f.value.type !== 'int') continue;
    const n = /** @type {number} */ (f.value.value);
    if (best === null || n < best) best = n;
  }
  return best;
}

/**
 * @typedef {{uuid: string, value: number|null}} Row
 * @typedef {{uuid: string, value: number}} Write
 */

/**
 * What `order on` writes: ties broken by +1 (cascading onto a position a
 * later row holds), then the unnumbered after the largest position, one
 * apart. `rows` is in the order the list showed, which decides ties and the
 * order of the unnumbered. Gaps are kept.
 * @param {Row[]} rows @returns {Write[]}
 */
export function numbering(rows) {
  /** @type {Write[]} */
  const writes = [];
  // Array.prototype.sort is stable: ties keep the order given.
  const numbered = rows
    .filter((r) => r.value !== null)
    .sort((a, b) => /** @type {number} */ (a.value) - /** @type {number} */ (b.value));
  let previous = null;
  for (const r of numbered) {
    let value = /** @type {number} */ (r.value);
    if (previous !== null && value <= previous) {
      value = previous + 1;
      writes.push({ uuid: r.uuid, value });
    }
    previous = value;
  }
  let next = (previous ?? 0) + 1;
  for (const r of rows) {
    if (r.value === null) writes.push({ uuid: r.uuid, value: next++ });
  }
  return writes;
}

/**
 * The daemon's order on the field: by position, a row without one last, then
 * by uuid (the hex form compares like the bytes).
 * @param {Row} a @param {Row} b
 */
export function compareRows(a, b) {
  if (a.value !== b.value) {
    if (a.value === null) return 1;
    if (b.value === null) return -1;
    return a.value - b.value;
  }
  return a.uuid < b.uuid ? -1 : a.uuid > b.uuid ? 1 : 0;
}

/**
 * The loaded rows once `writes` are applied, sorted again. With pages left
 * (`hasMore`), a row now sorted after the last row loaded leaves: it belongs
 * to a page not read yet, where the next page will bring it.
 * @param {Row[]} rows @param {Write[]} writes @param {boolean} hasMore
 * @returns {Row[]}
 */
export function reorder(rows, writes, hasMore) {
  const changed = new Map(writes.map((w) => [w.uuid, w.value]));
  const moved = rows.map((r) => (changed.has(r.uuid) ? { ...r, value: changed.get(r.uuid) ?? null } : r));
  const sorted = [...moved].sort(compareRows);
  if (!hasMore) return sorted;
  const unchanged = moved.filter((r) => !changed.has(r.uuid));
  const last = unchanged.reduce(
    (/** @type {Row|null} */ max, r) => (max === null || compareRows(r, max) > 0 ? r : max),
    null,
  );
  if (last === null) return sorted;
  return sorted.filter((r) => !changed.has(r.uuid) || compareRows(r, last) < 0);
}

/**
 * What moving `rows[index]` writes (empty: nothing to do). `max` is the largest
 * position of the whole list, which `bottom` needs and the loaded rows may not
 * hold.
 * @param {Row[]} rows @param {number} index
 * @param {'up'|'down'|'top'|'bottom'} where @param {number|null} max
 * @returns {Write[]}
 */
export function moveWrites(rows, index, where, max) {
  if (!MOVES.includes(where)) {
    throw new Error(`unknown move: "${where ?? ''}" (expected ${MOVES.join(' / ')})`);
  }
  const r = rows[index];
  if (!r) throw new Error('no row under the cursor');
  if (r.value === null) {
    throw new Error('this row has no position: turn the order on again to number it');
  }
  const value = r.value;
  /** @param {Row|undefined} n @param {number} untied @returns {Write[]} */
  const swap = (n, untied) => {
    if (!n || n.value === null) return [];
    if (n.value === value) return [{ uuid: r.uuid, value: untied }];
    return [
      { uuid: r.uuid, value: n.value },
      { uuid: n.uuid, value },
    ];
  };
  switch (where) {
    case 'up': {
      const n = rows[index - 1];
      return swap(n, (n?.value ?? 0) - 1);
    }
    case 'down': {
      const n = rows[index + 1];
      return swap(n, (n?.value ?? 0) + 1);
    }
    case 'top': {
      const first = rows[0];
      if (index === 0 || first.value === null) return [];
      return [{ uuid: r.uuid, value: first.value - 1 }];
    }
    default: {
      if (max === null) return [];
      const tied = rows[index + 1]?.value === value;
      if (value >= max && !tied) return [];
      return [{ uuid: r.uuid, value: Math.max(max, value) + 1 }];
    }
  }
}

/**
 * The writes as `query/fields/batch` ops, one `set` per metarecord.
 * @param {string} field @param {Write[]} writes
 */
function setOps(field, writes) {
  return writes.map((w) => ({
    op: 'set',
    query: { type: 'uuid_in', uuids: [w.uuid] },
    name: field,
    value: { type: 'int', value: w.value },
  }));
}

/**
 * The daemon side of the order mode, apart from the panel's DOM.
 * @param {{
 *   daemon: MetafolderApi['daemon'],
 *   getRepo: () => string|null,
 *   effectiveQuery: () => unknown,
 *   effectiveQueryText: () => string|null,
 *   getSort: () => {field: string, order: string}[],
 *   confirm?: (question: string) => boolean,
 *   pageSize?: number,
 * }} deps
 */
export function createOrderMode({
  daemon,
  getRepo,
  effectiveQuery,
  effectiveQueryText,
  getSort,
  confirm = (question) => window.confirm(question),
  pageSize = READ_PAGE,
}) {
  function repoOrThrow() {
    const repo = getRepo();
    if (!repo) throw new Error('no repository is open');
    return repo;
  }

  /** @param {string} repo @param {string} field @param {Write[]} writes */
  async function write(repo, field, writes) {
    const ops = setOps(field, writes);
    for (let i = 0; i < ops.length; i += BATCH_OPS) {
      await daemon.call('POST', `/repos/${repo}/query/fields/batch`, {
        ops: ops.slice(i, i + BATCH_OPS),
      });
    }
  }

  return {
    /**
     * `order on <field>`: numbers every match of the list's query. Resolves
     * with how many positions were written, or null when the user declined.
     * @param {string} field
     */
    async activate(field) {
      const repo = repoOrThrow();
      if (!field) throw new Error('order by which field?');
      const known = (await daemon.fields(repo)).find((f) => f.name === field);
      if (known && known.type !== 'int') {
        throw new Error(`"${field}" is a ${known.type} field: only an int field orders a list`);
      }
      const query = effectiveQuery() ?? MATCH_ALL;
      const sort = getSort();
      /** @type {Row[]} */
      const rows = [];
      /** @type {string|null} */
      let cursor = null;
      do {
        const page = await daemon.query(repo, {
          query,
          select: [field],
          limit: pageSize,
          ...(sort.length > 0 && { sort }),
          ...(cursor && { cursor }),
        });
        for (const m of page.records) rows.push({ uuid: m.uuid, value: positionOf(m, field) });
        cursor = page.nextCursor;
      } while (cursor);
      const writes = numbering(rows);
      if (writes.length === 0) return 0;
      if (writes.length > CONFIRM_ABOVE) {
        const text = effectiveQueryText();
        const what = text ? `the query\n  ${text}\n` : 'the whole repository (empty query)\n';
        const ok = confirm(
          `Order ${what}by "${field}"? ${writes.length} metarecords get a position written.`,
        );
        if (!ok) return null;
      }
      await write(repo, field, writes);
      return writes.length;
    },

    /**
     * `order move <where>` on `rows[index]`. Resolves with the loaded rows
     * reordered, or null when there was nothing to do.
     * @param {string} field @param {Row[]} rows @param {number} index
     * @param {'up'|'down'|'top'|'bottom'} where @param {boolean} hasMore
     */
    async move(field, rows, index, where, hasMore) {
      const repo = repoOrThrow();
      let max = null;
      if (where === 'bottom') {
        const page = await daemon.query(repo, {
          query: effectiveQuery() ?? MATCH_ALL,
          select: [field],
          limit: 1,
          sort: [{ field, order: 'desc' }],
        });
        const top = page.records[0];
        max = top ? positionOf(top, field) : null;
      }
      const writes = moveWrites(rows, index, where, max);
      if (writes.length === 0) return null;
      await write(repo, field, writes);
      return reorder(rows, writes, hasMore);
    },
  };
}
