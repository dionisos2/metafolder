// The checked multi-selection (doc "Cross-panel selection"), served at /__multi-select.js for panel types.
//
// Two workspace variables make it: `selected_metarecords` (createMultiSelect)
// and `selected_untracked_paths` (createPathSelect), the files the file
// manager checked that have no metarecord. A file is in one or the other,
// never both: one recorded meanwhile moves to the first (the file manager does
// it when its listing learns the metarecord).
//
// `selected_metarecords` is one workspace-wide set of metarecord uuids — the
// target of the bulk field operations (`metarecord:bulk`) and of
// `mf gui selected`. It is gathered wherever the user happens to be: rows
// checked in a metarecord-list stay checked when the list moves to another
// query, the file manager checks rows of its own into the same set, and every
// panel of the workspace mirrors the one variable.

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
 *   toggle: (uuid: string, mf?: MetafolderApi) => Promise<void>,
 *   add: (uuids: string[], mf?: MetafolderApi) => Promise<void>,
 *   clear: (mf?: MetafolderApi) => Promise<void>,
 *   pruneVanished: (mf?: MetafolderApi) => Promise<void>,
 * }}
 *   Each change takes the API of the command run making it, if one does (doc
 *   "Command transactions"), and writes through it.
 */
export function createMultiSelect({ workspace, daemon, render = () => {} }) {
  const mirror = createMirror(workspace, 'selected_metarecords', render);

  return {
    load: mirror.load,

    /** Whether one metarecord is checked. */
    has: mirror.has,

    /** How many metarecords are checked (in this list or not). */
    count: mirror.count,

    /** Every checked uuid, in check order. */
    values: mirror.values,

    /** Checks one metarecord, or unchecks it when it already is. */
    toggle: mirror.toggle,

    /** Checks these metarecords on top of what is already checked — the
     *  selection is gathered across lists, so "all" never drops earlier
     *  checks. */
    add: mirror.add,

    /** Unchecks everything. */
    clear: mirror.clear,

    /** Drops the checked metarecords that no longer exist. Nothing else is
     *  ever dropped: a checked row that merely left the current query is what
     *  "selecting in several lists" is made of. A vanished metarecord, though,
     *  could never be shown or unchecked again — one `uuid_in` query (a single
     *  node, one bitmap) says which of them are still
     *  there. No answer (the daemon down) keeps the selection as it is. */
    async pruneVanished(mf) {
      const list = mirror.values();
      const repo = mirror.repo();
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
      await mirror.keep((uuid) => alive.has(uuid), mf);
    },
  };
}

/**
 * The checked files without a metarecord, by absolute path — what the file
 * manager checks on an untracked entry. The file operations take them with the
 * checked metarecords' files (doc "Cross-panel selection"); the field
 * operations cannot.
 * @param {{workspace: Metafolder.Workspace, fs: Pick<Metafolder.Fs, 'exists'>,
 *   render?: () => void}} deps
 * @returns {{
 *   load: () => Promise<void>,
 *   has: (path: string) => boolean,
 *   count: () => number,
 *   values: () => string[],
 *   toggle: (path: string, mf?: MetafolderApi) => Promise<void>,
 *   add: (paths: string[], mf?: MetafolderApi) => Promise<void>,
 *   remove: (paths: string[], mf?: MetafolderApi) => Promise<void>,
 *   clear: (mf?: MetafolderApi) => Promise<void>,
 *   pruneVanished: (mf?: MetafolderApi) => Promise<void>,
 * }}
 */
export function createPathSelect({ workspace, fs, render = () => {} }) {
  const mirror = createMirror(workspace, 'selected_untracked_paths', render);
  return {
    load: mirror.load,
    has: mirror.has,
    count: mirror.count,
    values: mirror.values,
    toggle: mirror.toggle,
    add: mirror.add,
    /** Unchecks these paths — those recorded since, or trashed. */
    async remove(paths, mf) {
      const gone = new Set(paths);
      await mirror.keep((path) => !gone.has(path), mf);
    },
    clear: mirror.clear,
    /** Drops the paths no longer on disk (trashed, moved by a paste): they
     *  could never be shown or unchecked again. A path the disk does not
     *  answer about stays. */
    async pruneVanished(mf) {
      const list = mirror.values();
      if (list.length === 0) return;
      const present = await Promise.all(
        list.map(async (path) => {
          try {
            return await fs.exists(path);
          } catch {
            return true;
          }
        }),
      );
      const alive = new Set(list.filter((_, i) => present[i]));
      await mirror.keep((path) => alive.has(path), mf);
    },
  };
}

// The mirror of one workspace variable holding a list of strings.
//
// The delicate part. `workspace.set` is a plain write, and the backend
// announces every change to *every* instance of the workspace — the writing
// panel hears its own write back as an echo. An echo must therefore not
// re-adopt a value the panel has already moved past (two quick toggles write
// ['a'] then ['a','b']; the stale ['a'] echo arriving last would drop 'b'),
// while a change made by another panel or a script must be adopted at once. So
// every write remembers its payload until that payload comes back: an echo
// matching a pending write is ours and is dropped (the mirror already holds
// it), anything else is external and is taken.
/**
 * @param {Metafolder.Workspace} workspace @param {string} key
 * @param {() => void} render
 */
function createMirror(workspace, key, render) {
  /** @type {Set<string>} the checked values, in check order */
  let values = new Set();
  /** @type {string|null} the repository the selection belongs to */
  let repo = null;
  /** @type {Map<string, number>} payload JSON → writes still awaiting their echo */
  const pending = new Map();

  /** @param {unknown} value @returns {string[]} */
  const strings = (value) =>
    /** @type {string[]} */ ((Array.isArray(value) ? value : []).filter((v) => typeof v === 'string'));

  /** @param {string} json */
  function dropPending(json) {
    const left = (pending.get(json) ?? 1) - 1;
    if (left > 0) pending.set(json, left);
    else pending.delete(json);
  }

  /** Publishes `list` as the new state, through `mf` when a command run
   *  makes the change. @param {string[]} list @param {MetafolderApi} [mf] */
  async function write(list, mf) {
    const json = JSON.stringify(list);
    pending.set(json, (pending.get(json) ?? 0) + 1);
    values = new Set(list);
    render();
    try {
      await (mf?.workspace ?? workspace).set(key, list);
    } catch (error) {
      dropPending(json); // no echo is coming for a write that never landed
      throw error;
    }
  }

  workspace.onChange(key, (value) => {
    const list = strings(value);
    const json = JSON.stringify(list);
    if (pending.has(json)) {
      dropPending(json);
      return; // our own write echoing back: the mirror already holds it
    }
    values = new Set(list);
    render();
  });

  workspace.onChange('active_repo', (value) => {
    const next = typeof value === 'string' ? value : null;
    const previous = repo;
    repo = next;
    // The selection names things of one repository, so switching the
    // workspace to another one starts a new selection. Adopting a repository at
    // startup is not a switch: a (re)mounted panel keeps what the workspace
    // already checked.
    if (previous !== null && next !== previous) void write([]);
  });

  return {
    /** The repository the selection belongs to, null without one. */
    repo: () => repo,

    /** Adopts the workspace's current state — call once at mount. */
    load: async () => {
      const [activeRepo, selected] = await Promise.all([
        workspace.get('active_repo'),
        workspace.get(key),
      ]);
      repo = typeof activeRepo === 'string' ? activeRepo : null;
      values = new Set(strings(selected));
      render();
    },

    /** @param {string} value */
    has: (value) => values.has(value),

    count: () => values.size,

    values: () => [...values],

    /** Checks one value, or unchecks it when it already is.
     *  @param {string} value @param {MetafolderApi} [mf] */
    toggle: async (value, mf) => {
      const next = [...values];
      const at = next.indexOf(value);
      if (at >= 0) next.splice(at, 1);
      else next.push(value);
      await write(next, mf);
    },

    /** Checks these on top of what is already checked.
     *  @param {string[]} list @param {MetafolderApi} [mf] */
    add: async (list, mf) => {
      const next = new Set(values);
      const before = next.size;
      for (const value of list) {
        if (typeof value === 'string') next.add(value);
      }
      if (next.size === before) return; // nothing new: publish nothing
      await write([...next], mf);
    },

    /** Unchecks everything. @param {MetafolderApi} [mf] */
    clear: async (mf) => {
      if (values.size === 0) return; // already empty: publish nothing
      await write([], mf);
    },

    /** Keeps only the values `alive` says so of, publishing only a change.
     *  @param {(value: string) => boolean} alive @param {MetafolderApi} [mf] */
    keep: async (alive, mf) => {
      const list = [...values];
      const kept = list.filter(alive);
      if (kept.length === list.length) return;
      await write(kept, mf);
    },
  };
}
