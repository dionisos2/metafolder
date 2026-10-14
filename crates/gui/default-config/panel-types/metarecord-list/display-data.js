// What the list's rows show besides their own fields — tree paths, followed
// Ref targets, their paths, the watch state — read from the daemon per page
// and kept here, for the rows the panel displays only, with the marks drawn
// from them (orphan, unwatched, on an unplugged volume). Nothing is shared or
// cached: a change the feed reports makes the panel re-read it.
//
// What it needs from the panel arrives through `ctx`:
//   getRepo()     -> the active repository, or null
//   getColumns()  -> the columns shown (which fields to resolve)
//   exists(path)  -> whether a path is on disk (orphan check)
//   render()      -> repaint (when the mount points arrive)

import { fields } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { fetchMounts, offlineMountFor, relativeTo, unavailableLabel } from '/__mounts.js';
import { fetchWatched, summarizeWatched } from '/__watched.js';
import { fillColumns, treeRefFields, refTargetUuids, followedTreeFields } from '/__columns.js';

/**
 * A column spec, as ./columns.js parses it.
 * @typedef {import('/__columns.js').Column} Column
 *
 * Whether a metarecord's tracked file is gone, as /__orphan.js reports it.
 * @typedef {'deleted'|'missing'|null} OrphanState
 */

/**
 * Keyed display data, where a slower, older read never overwrites a newer
 * one: each read carries the sequence number of the prepare that made it.
 * `undefined` from `get` means *not read yet* — never *absent*.
 * @template T
 */
function createDisplayStore() {
  /** @type {Map<string, {seq: number, value: T}>} */
  const entries = new Map();
  return {
    /** @param {string} key @param {number} seq @param {T} value */
    put(key, seq, value) {
      const current = entries.get(key);
      if (!current || current.seq <= seq) entries.set(key, { seq, value });
    },
    /** @param {string} key @returns {T|undefined} */
    get: (key) => entries.get(key)?.value,
    clear: () => entries.clear(),
  };
}

/** @param {Metafolder.Metarecord} metarecord @param {string} field */
function hasTreeRef(metarecord, field) {
  return fields(metarecord, field).some((f) => f.value.type === 'tree_ref');
}

/**
 * @param {{
 *   daemon: MetafolderApi['daemon'],
 *   bench: MetafolderApi['bench'],
 *   exists: (path: string) => Promise<boolean>,
 *   getRepo: () => string|null,
 *   getColumns: () => Column[],
 *   render: () => void,
 * }} ctx
 */
export function createDisplayData({ daemon, bench, exists, getRepo, getColumns, render }) {
  /** @type {string|null} absolute root path of the active repo (read once) */
  let repoRoot = null;
  /** `${field}|${uuid}` → the metarecord's resolved paths in that TreeRef field
   *  (the displayed rows *and* the followed Ref targets). */
  /** @type {ReturnType<typeof createDisplayStore<string[]>>} */
  const treePaths = createDisplayStore();
  /** uuid → a followed Ref target (`null`: it does not exist). */
  /** @type {ReturnType<typeof createDisplayStore<Metafolder.Metarecord|null>>} */
  const targetRecords = createDisplayStore();
  let prepareSeq = 0;
  /** @param {string} field @param {string} uuid */
  const pathsIn = (field, uuid) => treePaths.get(`${field}|${uuid}`);
  /** @type {Map<string, Promise<OrphanState>>} uuid -> orphan state */
  let orphanCache = new Map();
  /** @type {Map<string, {watched: boolean, title: string}>} uuid -> the
   *  watch-state summary of the metarecord's paths, filled per page in
   *  prepareNow (one batch daemon call). A missing entry is "unknown" —
   *  no paths, or the daemon did not answer — and marks nothing. */
  let watchByUuid = new Map();
  /** @type {Array<import('/__mounts.js').Mount>} the repo's mount points,
   *  refreshed with the listing: a record on an unplugged volume is
   *  *unavailable*, never an orphan (doc "Unmounted volumes in the GUI"). */
  let mounts = [];

  const treeFieldsOf = () => new Set(['mfr_path', ...treeRefFields(getColumns())]);

  // Reads the display data the columns need, then fills them — rendering stays
  // synchronous and never mutates the metarecords themselves.
  /** @param {Metafolder.Metarecord[]} subset @returns {Promise<void>} */
  function prepare(subset) {
    return bench.measure('mf:list:enrich', () => prepareNow(subset));
  }

  /** @param {Metafolder.Metarecord[]} subset */
  async function prepareNow(subset) {
    const r = getRepo();
    if (subset.length === 0 || !r) return;
    const columns = getColumns();
    const seq = ++prepareSeq;
    if (repoRoot === null) repoRoot = await daemon.repoRoot(r);
    /** @param {string} field @param {string[]} uuids */
    const readTree = async (field, uuids) => {
      const byUuid = await daemon.treePaths(r, field, uuids).catch(() => null);
      if (!byUuid) return; // unanswered: those rows stay "not read yet"
      for (const uuid of uuids) {
        const paths = byUuid[uuid] ?? [];
        // A row's orphan state was judged from its path: a new path (its own
        // move, or an ancestor's rename) needs a new judgement.
        const before = pathsIn(field, uuid);
        if (field === 'mfr_path' && before && before.join('\n') !== paths.join('\n')) {
          orphanCache.delete(uuid);
        }
        treePaths.put(`${field}|${uuid}`, seq, paths);
      }
    };
    await Promise.all(
      [...treeFieldsOf()].map((field) =>
        readTree(
          field,
          subset.filter((m) => hasTreeRef(m, field)).map((m) => m.uuid),
        ),
      ),
    );
    const targetUuids = refTargetUuids(columns, subset);
    const targets = await daemon.metarecords(r, targetUuids).catch(() => null);
    if (targets) {
      for (const uuid of targetUuids) targetRecords.put(uuid, seq, targets.get(uuid) ?? null);
    }
    // Phase 2: `tag>path:path` columns also need the followed targets' own tree
    // paths resolved (same machinery, on the target uuids).
    await Promise.all(
      followedTreeFields(columns).map((field) =>
        readTree(
          field,
          targetUuids.filter((u) => {
            const t = targetRecords.get(u);
            return t != null && hasTreeRef(t, field);
          }),
        ),
      ),
    );
    // Phase 3: the watch state of the page's paths, one batch call (a daemon
    // without the endpoint — or a down one — leaves every row unmarked; a
    // fetch failure must not read as "unwatched"). The per-uuid summary is
    // "watched when any of its paths is", decided by /__watched.js.
    const watchRels = new Set();
    for (const m of subset) {
      if (!hasTreeRef(m, 'mfr_path')) continue;
      for (const rel of pathsIn('mfr_path', m.uuid) ?? []) watchRels.add(rel);
    }
    const byPath = await fetchWatched(daemon, r, [...watchRels]);
    for (const m of subset) {
      const rels = hasTreeRef(m, 'mfr_path') ? pathsIn('mfr_path', m.uuid) : [];
      if (rels === undefined || rels.length === 0) {
        watchByUuid.delete(m.uuid); // nothing to judge (a deleted record, say)
        continue;
      }
      const summary = summarizeWatched(rels.map((rel) => byPath.get(rel)));
      if (summary) watchByUuid.set(m.uuid, summary);
      else watchByUuid.delete(m.uuid);
    }
    fillFromDisplayData(subset);
  }

  /** @param {Metafolder.Metarecord[]} subset */
  function fillFromDisplayData(subset) {
    const columns = getColumns();
    /** @type {Record<string, Record<string, string[]>>} */
    const pathsByField = {};
    for (const field of treeFieldsOf()) {
      pathsByField[field] = {};
      for (const m of subset) {
        const paths = pathsIn(field, m.uuid);
        if (paths !== undefined) pathsByField[field][m.uuid] = paths;
      }
    }
    const targetUuids = refTargetUuids(columns, subset);
    /** @type {Map<string, Metafolder.Metarecord|null>} */
    const targets = new Map();
    for (const uuid of targetUuids) targets.set(uuid, targetRecords.get(uuid) ?? null);
    /** @type {Record<string, Record<string, string[]>>} */
    const followedPathsByField = {};
    for (const field of followedTreeFields(columns)) {
      followedPathsByField[field] = {};
      for (const uuid of targetUuids) {
        const paths = pathsIn(field, uuid);
        if (paths !== undefined) followedPathsByField[field][uuid] = paths;
      }
    }
    fillColumns(columns, subset, { pathsByField, targets, followedPathsByField });
  }

  // Absolute filesystem paths of a metarecord's mfr_path positions (read-only,
  // from the display data + the repo root).
  /** @param {Metafolder.Metarecord} metarecord @returns {string[]} */
  function pathsOf(metarecord) {
    const rootPath = repoRoot;
    if (!getRepo() || rootPath === null) return [];
    const rel = pathsIn('mfr_path', metarecord.uuid);
    if (rel === undefined) return [];
    // Resolved paths are leading-"/"-rooted ('' is the repo root itself), so the
    // absolute path is a plain concatenation with the repo root.
    return rel.map((p) => (p === '' ? rootPath : `${rootPath}${p}`));
  }

  // Whether the panel has read a metarecord's mfr_path positions. False until
  // the page's read lands (or when it failed): the gap is not evidence about
  // the file.
  /** @param {Metafolder.Metarecord} metarecord */
  function pathsResolved(metarecord) {
    return !!getRepo() && pathsIn('mfr_path', metarecord.uuid) !== undefined;
  }

  // Orphan check environment: paths are pre-resolved (enrich), so this is just a
  // disk stat (no daemon traffic during rendering).
  const orphanCtx = {
    // Async by contract (orphanState awaits it), though the paths are already
    // read with the page — the orphan check costs no daemon traffic.
    /** @param {Metafolder.Metarecord} metarecord */
    metarecordPaths: (metarecord) => Promise.resolve(pathsOf(metarecord)),
    exists,
  };

  /** Re-reads the repository's mount points (one cheap daemon call). */
  async function refreshMounts() {
    const asked = getRepo();
    const found = asked ? await fetchMounts(daemon, asked) : [];
    if (getRepo() === asked) {
      mounts = found;
      if (found.length > 0) render();
    }
  }

  /**
   * The offline mount point every path of the metarecord sits on, or null.
   * @param {Metafolder.Metarecord} metarecord
   */
  function unmountedFor(metarecord) {
    if (mounts.length === 0 || repoRoot === null) return null;
    const rootPath = repoRoot;
    const paths = pathsOf(metarecord);
    if (paths.length === 0) return null;
    const found = paths.map((abs) => offlineMountFor(mounts, relativeTo(rootPath, abs)));
    return found.every(Boolean) ? found[0] : null;
  }

  /** Marks the row/card when the metarecord's tracked file is gone (async).
   *  @param {HTMLElement} node @param {Metafolder.Metarecord} metarecord */
  function fillOrphan(node, metarecord) {
    // An unplugged volume comes first: its records' paths are stale on disk, so
    // the orphan check below would paint them as gone for good.
    const unmounted = unmountedFor(metarecord);
    if (unmounted) {
      node.classList.add('unmounted');
      node.title = unavailableLabel(unmounted);
      return;
    }
    // An mfr_path the panel has not read yet says nothing about the file.
    // Reading that gap as "no paths" would make the orphan check conclude the
    // file is missing and paint a healthy row as an orphan — leave it unmarked
    // and decide on a later render, once the path is known. Nothing is cached
    // either, so the verdict is genuinely re-taken.
    if (hasTreeRef(metarecord, 'mfr_path') && !pathsResolved(metarecord)) return;
    let state = orphanCache.get(metarecord.uuid);
    if (!state) {
      state = orphanState(metarecord, orphanCtx).catch(() => null);
      orphanCache.set(metarecord.uuid, state);
    }
    void state.then((resolved) => {
      if (resolved === null) return;
      node.classList.add('orphan');
      node.title = orphanLabel(resolved);
    });
  }

  /** Marks the row/card when the watcher will not record a change at the
   * tracked file: amber, with the reason on hover (doc "Checking whether a path is watched"). A
   * watched record — the normal case — stays unmarked; the reason
   *  is still on hover. The verdict comes from the page's batch fetch
   *  (prepareNow), so this is a synchronous read: an entry missing means
   *  "unknown" (no paths, or no answer) and marks nothing.
   *  @param {HTMLElement} node @param {Metafolder.Metarecord} metarecord */
  function fillWatched(node, metarecord) {
    if (!hasTreeRef(metarecord, 'mfr_path') || !pathsResolved(metarecord)) return;
    const info = watchByUuid.get(metarecord.uuid);
    if (!info) return;
    if (info.watched) {
      if (!node.title) node.title = info.title; // hover only: watched is the norm
      return;
    }
    // Yield to the stronger signals: an orphan (purple) and an unplugged
    // volume (amber, its own label) explain themselves better.
    if (node.classList.contains('orphan') || node.classList.contains('unmounted')) return;
    node.classList.add('unwatched');
    node.title = info.title;
  }

  return {
    prepare,
    /** Re-reads the display data of rows already shown (unmeasured: a
     *  repaint after a change, not a page load). */
    refresh: prepareNow,
    pathsOf,
    pathsResolved,
    refreshMounts,
    /** Draws a row's (or card's) marks. @param {HTMLElement} node
     *  @param {Metafolder.Metarecord} metarecord */
    mark(node, metarecord) {
      fillOrphan(node, metarecord);
      fillWatched(node, metarecord);
    },
    /** Forgets every row's display data (a new query). */
    clear() {
      orphanCache = new Map();
      watchByUuid = new Map();
      treePaths.clear();
      targetRecords.clear();
    },
    /** Forgets the repository too (another one became active). */
    reset() {
      this.clear();
      repoRoot = null;
      mounts = [];
    },
    /** Re-judges the orphan state of `uuids` (null: every row).
     *  @param {string[]|null} uuids */
    forgetOrphans(uuids) {
      if (uuids === null) orphanCache = new Map();
      else for (const u of uuids) orphanCache.delete(u);
    },
  };
}
