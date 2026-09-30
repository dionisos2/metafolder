// The checked multi-selection (doc "Cross-panel selection"), served at /__multi-select.js for panel types.
//
// `selected_metarecords` is one workspace-wide set of metarecord uuids — the
// target of the bulk field operations (`metarecord:bulk`) and of
// `mf gui selected`. It is gathered wherever the user happens to be: rows
// checked in a metarecord-list stay checked when the list moves to another
// query, the file manager checks rows of its own into the same set, and every
// panel of the workspace mirrors the one variable.
//
// The mirror is the delicate part. `workspace.set` is a plain write, and the
// backend announces every change to *every* instance of the workspace — the
// writing panel hears its own write back as an echo. An echo must therefore
// not re-adopt a value the panel has already moved past (two quick toggles
// write ['a'] then ['a','b']; the stale ['a'] echo arriving last would drop
// 'b'), while a change made by another panel or a script must be adopted at
// once. So every write remembers its payload until that payload comes back:
// an echo matching a pending write is ours and is dropped (the mirror already
// holds it), anything else is external and is taken.

/**
 * The one API method the pruning query needs.
 * @typedef {Pick<Metafolder.Daemon, 'call'>} Daemon
 */

/**
 * @param {{workspace: Metafolder.Workspace, daemon: Daemon, render?: () => void}} deps
 *   `render` repaints the panel's rows when the set changes (from anywhere).
 * @returns {{
 *   load: () => Promise<void>,
 *   has: (uuid: string) => boolean,
 *   count: () => number,
 *   values: () => string[],
 *   toggle: (uuid: string) => Promise<void>,
 *   add: (uuids: string[]) => Promise<void>,
 *   clear: () => Promise<void>,
 *   pruneVanished: () => Promise<void>,
 * }}
 */
export function createMultiSelect({ workspace, daemon, render = () => {} }) {
  /** @type {Set<string>} the checked uuids, in check order */
  let uuids = new Set();
  /** @type {string|null} the repository the selection belongs to */
  let repo = null;
  /** @type {Map<string, number>} payload JSON → writes still awaiting their echo */
  const pending = new Map();

  /** @param {string} json */
  function dropPending(json) {
    const left = (pending.get(json) ?? 1) - 1;
    if (left > 0) pending.set(json, left);
    else pending.delete(json);
  }

  /** Takes an external value as the new state (no write, no echo bookkeeping).
   *  @param {unknown} value */
  function adopt(value) {
    const list = /** @type {string[]} */ (
      (Array.isArray(value) ? value : []).filter((u) => typeof u === 'string')
    );
    uuids = new Set(list);
  }

  /** Publishes `list` as the new state. @param {string[]} list */
  async function write(list) {
    const json = JSON.stringify(list);
    pending.set(json, (pending.get(json) ?? 0) + 1);
    uuids = new Set(list);
    render();
    try {
      await workspace.set('selected_metarecords', list);
    } catch (error) {
      dropPending(json); // no echo is coming for a write that never landed
      throw error;
    }
  }

  workspace.onChange('selected_metarecords', (value) => {
    const list = /** @type {string[]} */ (
      (Array.isArray(value) ? value : []).filter((u) => typeof u === 'string')
    );
    const json = JSON.stringify(list);
    if (pending.has(json)) {
      dropPending(json);
      return; // our own write echoing back: the mirror already holds it
    }
    adopt(list);
    render();
  });

  workspace.onChange('active_repo', (value) => {
    const next = typeof value === 'string' ? value : null;
    const previous = repo;
    repo = next;
    // The selection names metarecords of one repository, so switching the
    // workspace to another one starts a new selection. Adopting a repository at
    // startup is not a switch: a (re)mounted panel keeps what the workspace
    // already checked.
    if (previous !== null && next !== previous) void write([]);
  });

  return {
    /** Adopts the workspace's current state — call once at mount. */
    async load() {
      const [activeRepo, selected] = await Promise.all([
        workspace.get('active_repo'),
        workspace.get('selected_metarecords'),
      ]);
      repo = typeof activeRepo === 'string' ? activeRepo : null;
      adopt(selected);
      render();
    },

    /** Whether one metarecord is checked. */
    has: (uuid) => uuids.has(uuid),

    /** How many metarecords are checked (in this list or not). */
    count: () => uuids.size,

    /** Every checked uuid, in check order. */
    values: () => [...uuids],

    /** Checks one metarecord, or unchecks it when it already is. */
    async toggle(uuid) {
      const next = [...uuids];
      const at = next.indexOf(uuid);
      if (at >= 0) next.splice(at, 1);
      else next.push(uuid);
      await write(next);
    },

    /** Checks these metarecords on top of what is already checked — the
     *  selection is gathered across lists, so "all" never drops earlier
     *  checks. */
    async add(list) {
      const next = new Set(uuids);
      const before = next.size;
      for (const uuid of list) {
        if (typeof uuid === 'string') next.add(uuid);
      }
      if (next.size === before) return; // nothing new: publish nothing
      await write([...next]);
    },

    /** Unchecks everything. */
    async clear() {
      if (uuids.size === 0) return; // already empty: publish nothing
      await write([]);
    },

    /** Drops the checked metarecords that no longer exist. Nothing else is
     *  ever dropped: a checked row that merely left the current query is what
     *  "selecting in several lists" is made of. A vanished metarecord, though,
     *  could never be shown or unchecked again — one `uuid_in` query (a single
     *  node, one bitmap) says which of them are still
     *  there. No answer (the daemon down) keeps the selection as it is. */
    async pruneVanished() {
      const list = [...uuids];
      if (list.length === 0 || repo === null) return;
      /** @type {{results?: unknown[]}|null} */
      let body;
      try {
        // No `select`: the daemon answers with the bare uuids it still holds.
        body = /** @type {{results?: unknown[]}|null} */ (
          await daemon.call('POST', `/repos/${repo}/query`, {
            query: { type: 'uuid_in', uuids: list },
            limit: list.length,
          })
        );
      } catch {
        return;
      }
      const alive = new Set(
        (body?.results ?? [])
          .map((row) =>
            typeof row === 'string' ? row : /** @type {{uuid?: string}|null} */ (row)?.uuid,
          )
          .filter((uuid) => typeof uuid === 'string'),
      );
      const kept = list.filter((uuid) => alive.has(uuid));
      if (kept.length === list.length) return;
      await write(kept);
    },
  };
}
