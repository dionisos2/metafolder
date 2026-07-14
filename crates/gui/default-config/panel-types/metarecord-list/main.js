// metarecord-list panel: metarecords of the active repo filtered by an embedded
// DSL query; primary selection source (spec-gui "metarecord-list panel type").

import { byId, el, fields, qs, thumbnail } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { createPagedList } from '/__paged-list.js';
import { createTypePicker, widgetFor, bulkSetBody, MATCH_ALL, createPickRunner } from '/__value-widget.js';
import { splitTerms, finderTargets, finderClause, composeQuery } from '/__finder.js';
import { attachHistory } from '/__history.js';
import { latestOnly } from '/__coalesce.js';
import {
  parseColumns,
  isSortable,
  cellQuickText,
  cellText,
  fillColumns,
  treeRefFields,
  refTargetUuids,
  followedTreeFields,
} from './columns.js';

// Smallest page of the three list panels: each row needs several daemon
// round-trips (TreeRef path resolution, ref-target metarecords) and parsing,
// so a modest page keeps a large result responsive on first display. The
// effective default comes from the GUI config (`[page-size].metarecord-list`);
// this is only the fallback, and the per-workspace page-size variable still
// overrides it.
const DEFAULT_PAGE_SIZE_FALLBACK = 100;
const DEFAULT_COLUMNS = 'mfr_path:path mfr_type &version';
// Fields the finder (quick OSM filter) searches by default, each with an
// explicit mode (`field:path` for the tree_ref path, `field:direct` for a plain
// value) so it never depends on the async field catalog. A bare `field` (no
// mode) auto-detects from the catalog. Missing fields contribute nothing.
// Overridable per workspace via `metarecord-list:finder-fields`.
const DEFAULT_FINDER_FIELDS = ['mfr_path:path', 'label:direct', 'name:direct'];
// Idle delay before the finder re-runs the query, so a burst of typing sends
// one request rather than one per keystroke.
const FINDER_DEBOUNCE_MS = 500;
const GRID_NAME_COLUMN = parseColumns('mfr_path:path')[0];

/**
 * A column spec, as ./columns.js parses it.
 * @typedef {import('./columns.js').Column} Column
 *
 * A sort key, as the daemon's query body takes it.
 * @typedef {{field: string, order: 'asc'|'desc'}} SortKey
 *
 * The value editor `widgetFor` builds.
 * @typedef {{element: HTMLElement, read: () => Metafolder.Value}} Widget
 *
 * Whether a metarecord's tracked file is gone, as /__orphan.js reports it.
 * @typedef {'deleted'|'missing'|null} OrphanState
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, query, bench, cache } = metafolder;
  // Annotated: an unannotated `const x = cache.REFRESH` widens the unique symbol
  // to plain `symbol`, and `value === x` then narrows nothing.
  /** @type {Metafolder.Refresh} */
  const REFRESH = cache.REFRESH;
  const defaultPageSize = metafolder.pageSize ?? DEFAULT_PAGE_SIZE_FALLBACK;
  // UX timing knobs (config.toml `[panels]`), with the module fallbacks below.
  const { settings } = metafolder;
  const finderDebounceMs = settings.finderDebounceMs ?? FINDER_DEBOUNCE_MS;
  const livePreviewMs = settings.livePreviewDebounceMs ?? 130;
  const statusMessageMs = settings.statusMessageMs ?? 5000;

  /** @type {string|null} */
  let repo = null;
  /** @type {Column[]} persisted per workspace (spec strings) */
  let columns = parseColumns(DEFAULT_COLUMNS);
  /** @type {Record<string, number>} column spec -> px; persisted per workspace */
  let widths = {};
  /** @type {Metafolder.Metarecord[]} */
  let metarecords = [];
  /** @type {string|null} */
  let nextCursor = null;
  /** @type {number|null} full result count (daemon-side COUNT, first page only) */
  let total = null;
  let pageSize = defaultPageSize; // persisted per workspace
  let loading = false;
  /** @type {Record<string, unknown>|null} null = match all (the structural base query) */
  let queryIR = null;
  let finderText = ''; // quick OSM filter, AND-ed onto the base query
  /** @type {string[]} */
  let finderFields = DEFAULT_FINDER_FIELDS.slice();
  /** @type {ReturnType<typeof setTimeout>|undefined} */
  let finderTimer;
  let normalShown = false; // zone B (normal DSL) revealed?
  let normalFrozen = false; // zone B decoupled (hand-edited, authoritative)?
  let queryInitialized = false; // first query compiled on first display
  // Opening a repo does not run the query automatically (often nothing is
  // wanted from this panel yet): the list stays empty until the user runs it
  // — apply the query, type in the finder, sort, or refresh. Reset on repo
  // change; a value-picker opening (pick_request) arms it, rows are needed.
  let queryRan = false;
  /** @type {ReturnType<typeof setTimeout>|undefined} */
  let livePreviewTimer;
  /** @type {SortKey[]} */
  let sort = [];
  let cursorIndex = -1;
  /** @type {Set<string>} multi-selection (uuids) */
  let checked = new Set();
  let mode = 'table';
  /** @type {Map<string, Promise<OrphanState>>} uuid -> orphan state */
  let orphanCache = new Map();

  const bodyEl = qs(root, '.mf-panel-body');
  const rows = byId(root, 'rows');
  const grid = byId(root, 'grid');
  const scroll = byId(root, 'scroll');
  const statusLine = byId(root, 'status-line');
  const finderInput = byId(root, 'finder-input', HTMLInputElement);
  const finderFieldsLabel = byId(root, 'finder-fields');
  const queryInput = byId(root, 'query-input', HTMLInputElement);
  const columnsInput = byId(root, 'columns-input', HTMLInputElement);
  const queryError = byId(root, 'query-error');
  const columnsError = byId(root, 'columns-error');
  const normalToggle = byId(root, 'normal-toggle');
  const normalEditor = byId(root, 'normal-editor');
  const normalInput = byId(root, 'normal-input', HTMLTextAreaElement);
  const normalError = byId(root, 'normal-error');
  const normalFreeze = byId(root, 'normal-freeze', HTMLInputElement);
  const bulkForm = byId(root, 'bulk-form');
  const bulkOp = byId(root, 'bulk-op', HTMLSelectElement);
  const bulkName = byId(root, 'bulk-name', HTMLInputElement);
  const bulkValueSlot = byId(root, 'bulk-value');
  const bulkForce = byId(root, 'bulk-force', HTMLInputElement);
  const bulkError = byId(root, 'bulk-error');

  // Per-repo input history (spec-gui "Input history"): ctrl-p/ctrl-n walk,
  // ctrl-r OSM search. Recorded on explicit submits only, never the debounce.
  const historyDeps = {
    /** @param {string} histRepo @param {string} zone */
    read: (histRepo, zone) => metafolder.history.read(histRepo, zone),
    /** @param {string} histRepo @param {string} zone @param {string} entry */
    append: (histRepo, zone, entry) => metafolder.history.append(histRepo, zone, entry),
    getRepo: async () => repo,
    container: bodyEl,
  };
  const finderHistory = attachHistory(finderInput, {
    zone: 'metarecord-list:finder',
    ...historyDeps,
  });
  const queryHistory = attachHistory(queryInput, { zone: 'metarecord-list:query', ...historyDeps });

  // ── Data access (all daemon data comes from the shared cache) ─────────────

  /** @type {string|null} absolute root path of the active repo (cached once) */
  let repoRoot = null;

  /** @param {Metafolder.Metarecord} metarecord @param {string} field */
  function hasTreeRef(metarecord, field) {
    return fields(metarecord, field).some((f) => f.value.type === 'tree_ref');
  }

  const treeFieldsOf = () => new Set(['mfr_path', ...treeRefFields(columns)]);

  // Pre-fetches the display data for the ~ columns into the shared cache, then
  // fills the columns from cache reads — rendering stays synchronous and never
  // mutates the (shared, read-only) cached metarecords.
  /** @param {Metafolder.Metarecord[]} subset @returns {Promise<void>} */
  function prepare(subset) {
    return bench.measure('mf:list:enrich', () => prepareNow(subset));
  }

  /** @param {Metafolder.Metarecord[]} subset */
  async function prepareNow(subset) {
    // Held in a const: `repo` is a captured `let`, so a guard on it does not
    // narrow inside the callbacks below.
    const r = repo;
    if (subset.length === 0 || !r) return;
    if (repoRoot === null) repoRoot = await daemon.repoRoot(r);
    await Promise.all(
      [...treeFieldsOf()].map((field) =>
        cache.fetchTreeRefs(
          r,
          field,
          subset.filter((m) => hasTreeRef(m, field)).map((m) => m.uuid),
        ),
      ),
    );
    const targetUuids = refTargetUuids(columns, subset);
    await cache.fetchMetarecords(r, targetUuids);
    // Phase 2: `tag>path:path` columns also need the followed targets' own tree
    // paths resolved (same machinery, on the target uuids).
    await Promise.all(
      followedTreeFields(columns).map((field) =>
        cache.fetchTreeRefs(
          r,
          field,
          targetUuids.filter((u) => {
            const t = cache.readMetarecord(r, u);
            return t !== REFRESH && hasTreeRef(t, field);
          }),
        ),
      ),
    );
    fillFromCache(subset);
  }

  /** @param {Metafolder.Metarecord[]} subset */
  function fillFromCache(subset) {
    const r = repo;
    if (!r) return;
    /** @type {Record<string, Record<string, string[]>>} */
    const pathsByField = {};
    for (const field of treeFieldsOf()) {
      pathsByField[field] = {};
      for (const m of subset) {
        const paths = cache.readTreeRef(r, field, m.uuid);
        if (paths !== REFRESH) pathsByField[field][m.uuid] = paths;
      }
    }
    const targetUuids = refTargetUuids(columns, subset);
    /** @type {Map<string, Metafolder.Metarecord|null>} */
    const targets = new Map();
    for (const uuid of targetUuids) {
      const target = cache.readMetarecord(r, uuid);
      targets.set(uuid, target === REFRESH ? null : target);
    }
    /** @type {Record<string, Record<string, string[]>>} */
    const followedPathsByField = {};
    for (const field of followedTreeFields(columns)) {
      followedPathsByField[field] = {};
      for (const uuid of targetUuids) {
        const paths = cache.readTreeRef(r, field, uuid);
        if (paths !== REFRESH) followedPathsByField[field][uuid] = paths;
      }
    }
    fillColumns(columns, subset, { pathsByField, targets, followedPathsByField });
  }

  // Absolute filesystem paths of a metarecord's mfr_path positions (read-only,
  // from the cache + the repo root) — replaces the old per-metarecord `.paths`.
  /** @param {Metafolder.Metarecord} metarecord @returns {string[]} */
  function pathsOf(metarecord) {
    const r = repo;
    const rootPath = repoRoot;
    if (!r || rootPath === null) return [];
    const rel = cache.readTreeRef(r, 'mfr_path', metarecord.uuid);
    if (rel === REFRESH) return [];
    return rel.map((p) => (p === '' ? rootPath : `${rootPath}/${p}`));
  }

  /** Re-derives the ~ columns over the loaded metarecords (after a column change). */
  async function reresolveColumns() {
    await prepareNow(metarecords);
  }

  // The query actually run: the structural base query AND the finder's OSM
  // clause (mode auto-detected per field from the catalog). null = match all.
  function effectiveQuery() {
    const r = repo;
    const targets = finderTargets(finderFields, (f) => (r ? cache.fieldType(r, f) : null));
    return composeQuery(queryIR, finderClause(splitTerms(finderText), targets));
  }

  // Returns false when the call is dropped (no repo, or another fetch is in
  // flight — fetches are serialized on `loading`), so the finder can re-run the
  // latest query instead of leaving the list stale.
  /** @param {boolean} reset @returns {Promise<boolean|undefined>} */
  async function fetchPage(reset) {
    const r = repo;
    if (!r || loading) return false;
    loading = true;
    try {
      // A reset fetch is a deliberate freshness point (query, refresh, display):
      // poll the change feed so stale cached data is dropped before we read.
      if (reset) await cache.sync(r);
      /** @type {string|null} */
      let keepUuid = null;
      if (reset) {
        // A refresh must not steal the selection: remember the highlighted
        // metarecord and restore it.
        const previous = /** @type {{uuid: string}|null} */ (
          (await workspace.get('selected_metarecord')) ?? null
        );
        keepUuid = metarecords[cursorIndex]?.uuid ?? previous?.uuid ?? null;
        metarecords = [];
        nextCursor = null;
        orphanCache = new Map();
      }
      let result;
      try {
        result = await cache.query(r, {
          query: effectiveQuery() ?? MATCH_ALL,
          select: '*',
          limit: pageSize,
          ...(reset && { count: true }), // daemon-side COUNT, no extra pages
          ...(sort.length > 0 && { sort }),
          ...(nextCursor && { cursor: nextCursor }),
        });
      } catch (error) {
        await statusBar.error(error);
        return;
      }
      // The page's metarecords are read from the cache the query just populated.
      const fetched = /** @type {Metafolder.Metarecord[]} */ (
        result.uuids.map((u) => cache.readMetarecord(r, u)).filter((m) => m !== REFRESH)
      );
      metarecords = metarecords.concat(fetched);
      nextCursor = result.nextCursor;
      await prepare(fetched); // pre-resolve display data; rendering stays sync
      if (reset) total = result.total;
      if (reset) {
        // Drop checked metarecords that no longer match.
        const alive = new Set(metarecords.map((e) => e.uuid));
        if ([...checked].some((uuid) => !alive.has(uuid))) {
          checked = new Set([...checked].filter((uuid) => alive.has(uuid)));
          await workspace.set('selected_metarecords', [...checked]);
        }
        const keepIndex =
          keepUuid === null ? -1 : metarecords.findIndex((e) => e.uuid === keepUuid);
        if (keepIndex >= 0) {
          cursorIndex = keepIndex;
        } else if (metarecords.length > 0) {
          render();
          await setCursor(0);
          return;
        } else {
          cursorIndex = -1;
        }
      }
      render();
    } finally {
      loading = false;
    }
  }

  // ── Rendering ─────────────────────────────────────────────────────────

  /** @type {{column: Column, startX: number, startWidth: number, moved: boolean}|null} */
  let resizing = null;

  /** @param {MouseEvent} event @param {Column} column */
  function startResize(event, column) {
    event.preventDefault();
    const th = /** @type {HTMLElement} */ (event.target).closest('th');
    if (!th) return;
    resizing = { column, startX: event.clientX, startWidth: th.offsetWidth, moved: false };
  }

  // Document-level so the drag keeps tracking outside the column; removed on
  // unmount (cleanup) so they do not leak across panel instances.
  /** @param {MouseEvent} event */
  const onMouseMove = (event) => {
    if (!resizing) return;
    resizing.moved = true;
    widths[resizing.column.spec] = Math.max(
      40,
      resizing.startWidth + event.clientX - resizing.startX,
    );
    renderHeader();
  };
  const onMouseUp = () => {
    if (!resizing) return;
    const { moved } = resizing;
    resizing = null;
    if (moved) void workspace.set('metarecord-list:column-widths', { ...widths });
  };
  document.addEventListener('mousemove', /** @type {EventListener} */ (onMouseMove));
  document.addEventListener('mouseup', onMouseUp);

  function renderHeader() {
    byId(root, 'header-row').replaceChildren(
      ...columns.map((column) => {
        const active = isSortable(column) ? sort.find((s) => s.field === column.name) : undefined;
        const th = el(
          'th',
          { onclick: () => toggleSort(column) },
          column.spec + (active ? (active.order === 'asc' ? ' ▲' : ' ▼') : ''),
          el('div', {
            class: 'col-resize',
            onmousedown: (/** @type {MouseEvent} */ event) => startResize(event, column),
            onclick: (/** @type {Event} */ event) => event.stopPropagation(),
          }),
        );
        if (widths[column.spec]) th.style.width = `${widths[column.spec]}px`;
        return th;
      }),
    );
  }

  // Orphan check environment: paths are pre-resolved (enrich), so this is just a
  // disk stat (no daemon traffic during rendering).
  const orphanCtx = {
    // Async by contract (orphanState awaits it), though the paths are already
    // resolved in the cache — the orphan check costs no daemon traffic.
    /** @param {Metafolder.Metarecord} metarecord */
    metarecordPaths: (metarecord) => Promise.resolve(pathsOf(metarecord)),
    /** @param {string} path */
    exists: (path) =>
      metafolder.fs.stat(path).then(
        () => true,
        () => false,
      ),
  };

  /** Marks the row/card when the metarecord's tracked file is gone (async).
   *  @param {HTMLElement} node @param {Metafolder.Metarecord} metarecord */
  function fillOrphan(node, metarecord) {
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

  function render() {
    bench.measure('mf:list:render', renderNow);
  }

  function renderNow() {
    renderHeader();
    rows.replaceChildren(
      ...metarecords.map((metarecord, index) => {
        const tr = el(
          'tr',
          {
            class: [
              'row',
              index === cursorIndex && 'cursor',
              checked.has(metarecord.uuid) && 'checked',
            ],
            onclick: () => setCursor(index),
            ondblclick: () => openSelected(),
          },
          columns.map((column) => el('td', {}, cellText(column, metarecord))),
        );
        fillOrphan(tr, metarecord);
        return tr;
      }),
    );
    grid.replaceChildren(
      ...metarecords.map((metarecord, index) => {
        const card = el(
          'div',
          {
            class: ['card', index === cursorIndex && 'cursor'],
            onclick: () => setCursor(index),
            ondblclick: () => openSelected(),
          },
          thumbnail(metafolder.guiServer, pathsOf(metarecord)[0], {
            glyphClass: 'glyph',
            token: metafolder.sessionToken,
          }),
          el(
            'div',
            { class: 'name' },
            cellQuickText(GRID_NAME_COLUMN, metarecord) || metarecord.uuid.slice(0, 8),
          ),
        );
        fillOrphan(card, metarecord);
        return card;
      }),
    );
    statusLine.textContent = !queryRan
      ? repo === null
        ? ''
        : 'query not run — apply the query/finder (Enter) or refresh to load the list'
      : `${metarecords.length}${total !== null ? `/${total}` : ''} metarecord${
          (total ?? metarecords.length) === 1 ? '' : 's'
        }` +
        (nextCursor ? ' (more available — scroll down)' : '') +
        (checked.size > 0 ? ` — ${checked.size} selected` : '');
  }

  // ── Selection (workspace variables) ─────────────────────────────────────

  // Held-arrow navigation must stay cheaper than the key-repeat rate, or key
  // events accumulate and keep replaying after release: moving the cursor only
  // retargets the `.cursor` class (the rows are index-aligned with
  // `metarecords`, no re-render), and the selection propagation (two
  // workspace.set IPC round-trips fanning out to the other panels) is
  // coalesced — one in flight, one trailing with the final position.
  function moveCursorHighlight() {
    for (const container of [rows, grid]) {
      container.querySelector('.cursor')?.classList.remove('cursor');
      container.children[cursorIndex]?.classList.add('cursor');
    }
  }

  const propagateSelection = latestOnly(async () => {
    const metarecord = metarecords[cursorIndex];
    if (!metarecord) return;
    await workspace.set('selected_metarecord', { uuid: metarecord.uuid, repo });
    await workspace.set('selected_paths', pathsOf(metarecord));
  });

  /** @param {number} index */
  async function setCursor(index) {
    cursorIndex = Math.max(0, Math.min(index, metarecords.length - 1));
    if (!metarecords[cursorIndex]) {
      render();
      return;
    }
    moveCursorHighlight();
    root.querySelector('tr.cursor')?.scrollIntoView({ block: 'nearest' });
    await propagateSelection();
  }

  async function toggleChecked() {
    const metarecord = metarecords[cursorIndex];
    if (!metarecord) return;
    if (checked.has(metarecord.uuid)) checked.delete(metarecord.uuid);
    else checked.add(metarecord.uuid);
    render();
    await workspace.set('selected_metarecords', [...checked]);
  }

  async function clearChecked() {
    if (checked.size === 0) return;
    checked = new Set();
    render();
    await workspace.set('selected_metarecords', []);
  }

  async function openSelected() {
    const metarecord = metarecords[cursorIndex];
    if (!metarecord) return;
    const paths = pathsOf(metarecord);
    await commands.invoke(`panel:reveal-other ${paths.length > 0 ? 'file' : 'metarecord-detail'}`);
  }

  // ── Query (two-zone editor) ─────────────────────────────────────────────

  /** Recomputes `queryIR` from the current editor state (no fetch). */
  async function recomputeQuery() {
    queryError.textContent = '';
    normalError.textContent = '';
    /** @type {string} */
    let dsl;
    if (normalShown && normalFrozen) {
      dsl = normalInput.value.trim(); // frozen normal DSL is authoritative
    } else {
      const simplified = queryInput.value.trim();
      if (simplified === '') {
        dsl = '';
      } else {
        try {
          dsl = String(await query.expand(simplified)).trim();
        } catch (error) {
          queryError.textContent = messageOf(error);
          return false;
        }
      }
      if (normalShown) normalInput.value = dsl; // reflect in B
    }
    if (dsl === '') {
      queryIR = null; // empty = match all
    } else {
      try {
        queryIR = /** @type {Record<string, unknown>} */ (await query.parse(dsl));
      } catch (error) {
        (normalShown ? normalError : queryError).textContent = messageOf(error);
        return false;
      }
    }
    return true;
  }

  async function applyQuery() {
    queryRan = true;
    queryHistory.push(queryInput.value.trim());
    const ok = await recomputeQuery();
    await persistQueryState();
    if (ok) await fetchPage(true);
  }

  async function persistQueryState() {
    await workspace.set('metarecord-list:query', queryInput.value);
    await workspace.set('metarecord-list:normal-query', normalInput.value);
  }

  /** Debounced live mirror of expand(A) into B (preview only — does not run). */
  function scheduleLivePreview() {
    if (!normalShown || normalFrozen) return;
    clearTimeout(livePreviewTimer);
    livePreviewTimer = setTimeout(() => void refreshPreview(), livePreviewMs);
  }

  async function refreshPreview() {
    if (!normalShown || normalFrozen) return;
    queryError.textContent = '';
    const simplified = queryInput.value.trim();
    if (simplified === '') {
      normalInput.value = '';
      return;
    }
    try {
      normalInput.value = String(await query.expand(simplified)).trim();
    } catch (error) {
      queryError.textContent = messageOf(error);
    }
  }

  /** @param {boolean} shown */
  async function setNormalShown(shown) {
    normalShown = shown;
    normalEditor.hidden = !shown;
    normalToggle.textContent = shown ? 'Hide normal DSL' : 'Show normal DSL';
    if (shown && !normalFrozen) await refreshPreview();
    await workspace.set('metarecord-list:normal-shown', shown);
  }

  /** @param {boolean} frozen */
  async function setNormalFrozen(frozen) {
    normalFrozen = frozen;
    normalFreeze.checked = frozen;
    normalInput.readOnly = !frozen;
    if (!frozen && normalShown) await refreshPreview();
    await workspace.set('metarecord-list:normal-frozen', frozen);
  }

  // ── Columns ─────────────────────────────────────────────────────────────

  /** @param {unknown} value the persisted `metarecord-list:columns` variable */
  function setColumns(value) {
    /** @type {Column[]} */
    let parsed = [];
    try {
      parsed = parseColumns(Array.isArray(value) ? value.join(' ') : '');
    } catch {
      /* stale persisted value: fall back to the defaults */
    }
    columns = parsed.length > 0 ? parsed : parseColumns(DEFAULT_COLUMNS);
    columnsInput.value = columns.map((c) => c.spec).join(' ');
  }

  /** Applies the columns input (no daemon round-trip: select is always '*'). */
  async function applyColumns() {
    columnsError.textContent = '';
    /** @type {Column[]} */
    let parsed;
    try {
      parsed = parseColumns(columnsInput.value);
    } catch (error) {
      columnsError.textContent = messageOf(error);
      return;
    }
    columns = parsed.length > 0 ? parsed : parseColumns(DEFAULT_COLUMNS);
    columnsInput.value = columns.map((c) => c.spec).join(' ');
    await reresolveColumns();
    render();
    await workspace.set(
      'metarecord-list:columns',
      columns.map((c) => c.spec),
    );
  }

  /** A stored/typed page size; anything invalid falls back to the default. */
  /** @param {unknown} value */
  function sanitizePageSize(value) {
    const n = Math.floor(Number(value));
    return Number.isFinite(n) && n >= 1 ? n : defaultPageSize;
  }

  /** @param {Column} column */
  function toggleSort(column) {
    if (!isSortable(column)) return; // metarecord meta, not a sortable field
    const current = sort.find((s) => s.field === column.name);
    sort = current
      ? current.order === 'asc'
        ? [{ field: column.name, order: 'desc' }]
        : []
      : [{ field: column.name, order: 'asc' }];
    queryRan = true;
    void fetchPage(true);
  }

  // ── Bulk edit (set/append/remove a field over the whole query result) ────

  /** @type {Widget|null} the value editor following the picked type */
  let bulkWidget = null;

  // Each operation maps to its batch endpoint and a confirmation verb.
  // `valueless` ops (unset) act on the field name alone — no value widget.
  /** @type {Record<string, {path: string, verb: string, prep: string, valueless?: boolean}>} */
  const BULK_OPS = {
    set: { path: 'query/fields/set', verb: 'Set', prep: 'on' },
    append: { path: 'query/fields/append', verb: 'Append', prep: 'to' },
    remove: { path: 'query/fields/remove', verb: 'Remove', prep: 'from' },
    unset: { path: 'query/fields/unset', verb: 'Unset', prep: 'from', valueless: true },
  };

  // Value picker (spec-gui "Value picker") for the bulk-set value widget.
  const pickRunner = createPickRunner(metafolder);
  const bulkPickOpts = {
    /** @param {string} valueType */
    pick: (valueType) => pickRunner.run({ field: bulkName.value.trim(), valueType }),
  };

  /** The form's value widget follows the picked type.
   *  @param {string} type */
  function setBulkWidget(type) {
    bulkWidget = widgetFor(type, undefined, bulkPickOpts);
    bulkValueSlot.replaceChildren(bulkWidget.element);
  }
  const bulkTypePicker = createTypePicker(byId(root, 'bulk-type'), 'string', setBulkWidget);
  setBulkWidget(bulkTypePicker.get());

  // Hide the type picker + value row for value-less ops (unset).
  const bulkValueRow = byId(root, 'bulk-value-row');
  const bulkTypeBtn = byId(root, 'bulk-type');
  function syncBulkOpUi() {
    const valueless = (BULK_OPS[bulkOp.value] ?? BULK_OPS.set).valueless === true;
    bulkValueRow.hidden = valueless;
    bulkTypeBtn.hidden = valueless;
  }
  bulkOp.addEventListener('change', syncBulkOpUi);
  syncBulkOpUi();

  function openBulkForm() {
    bulkError.textContent = '';
    bulkForm.classList.add('open');
    bulkName.focus();
  }

  /** Counts the metarecords the current query matches (for the confirmation). */
  async function countMatches() {
    const result = /** @type {{total?: number|null}} */ (
      await daemon.call('POST', `/repos/${repo}/query`, {
        query: effectiveQuery() ?? MATCH_ALL,
        select: '*',
        limit: 1,
        count: true,
      })
    );
    return result.total ?? 0;
  }

  async function applyBulkEdit() {
    bulkError.textContent = '';
    try {
      if (!repo) throw new Error('no active repository');
      const op = BULK_OPS[bulkOp.value] ?? BULK_OPS.set;
      const name = bulkName.value.trim();
      if (!name) throw new Error('field name is required');
      const force = name.startsWith('mfr_') || bulkForce.checked;
      const n = await countMatches();
      if (!confirm(`${op.verb} "${name}" ${op.prep} ${n} metarecord${n === 1 ? '' : 's'}?`)) return;
      // Value-less ops (unset) act on the name alone. Bulk edits target the
      // effective (finder-filtered) set — you act on what you see.
      const effQ = effectiveQuery();
      const widget = bulkWidget;
      if (!op.valueless && !widget) throw new Error('no value widget');
      const body =
        op.valueless || !widget
          ? { query: effQ ?? MATCH_ALL, name, ...(force ? { force: true } : {}) }
          : bulkSetBody(effQ, name, widget.read(), force);
      const resp = /** @type {{updated?: number}} */ (
        await daemon.call('POST', `/repos/${repo}/${op.path}`, body)
      );
      const updated = resp.updated ?? 0;
      bulkForm.classList.remove('open');
      void statusBar.message(
        `${op.verb} "${name}": ${updated} metarecord${updated === 1 ? '' : 's'} changed.`,
        statusMessageMs,
      );
      // Refresh this list and any metarecord-detail mirror (the cache picks the
      // write up via the change feed on the next reset fetch).
      await workspace.set('metarecords:dirty', Date.now());
    } catch (error) {
      bulkError.textContent = messageOf(error);
    }
  }

  // Progressive loading: the shared controller owns the scroll threshold and
  // the one-fetch-at-a-time guard; the footer below stays custom (it carries
  // the selection count too). hasMore tracks the daemon cursor.
  const pager = createPagedList({
    loaded: () => metarecords.length,
    total: () => total,
    hasMore: () => nextCursor !== null,
    loadMore: async () => {
      await fetchPage(false); // the pager wants no result, only completion
    },
  });
  const detachScroll = pager.attach(scroll);

  byId(root, 'query-apply').addEventListener('click', () => {
    void commands.invoke('metarecord-list:apply-query');
  });
  byId(root, 'columns-apply').addEventListener('click', () => {
    void commands.invoke('metarecord-list:apply-columns');
  });
  queryInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyQuery();
  });
  queryInput.addEventListener('input', scheduleLivePreview);

  // ── Finder (quick OSM filter) ─────────────────────────────────────────────

  function updateFinderFieldsLabel() {
    const names = finderFields.map((e) => e.split(':')[0]);
    finderFieldsLabel.textContent = names.join(' ');
    finderFieldsLabel.title = `finder searches: ${finderFields.join(', ')} (osm path / osmd direct)`;
  }

  /** Re-runs the query for the current finder text (debounced on input).
   *  Fetches are serialized (the `loading` guard drops concurrent calls), so a
   *  fast typist can outrun an in-flight fetch and leave the list showing an
   *  earlier term. Re-run when our fetch was dropped, or the input moved on
   *  while we were fetching, until the shown list matches the current input. */
  /** @param {{record?: boolean}} [options] */
  async function applyFinder({ record = false } = {}) {
    clearTimeout(finderTimer);
    queryRan = true;
    if (record) finderHistory.push(finderInput.value.trim());
    finderText = finderInput.value;
    await workspace.set('metarecord-list:finder', finderText);
    const ran = await fetchPage(true);
    if (repo && (ran === false || finderInput.value !== finderText)) {
      finderTimer = setTimeout(() => void applyFinder(), 80);
    }
  }

  function scheduleFinder() {
    clearTimeout(finderTimer);
    finderTimer = setTimeout(() => void applyFinder(), finderDebounceMs);
  }

  finderInput.addEventListener('input', scheduleFinder);
  // The finder's in-input shortcuts (arrows move the selection, Ctrl+Enter
  // confirms a pick, Enter re-runs the filter, Escape leaves it) are declared in
  // keybindings.toml with `focus = "finder"` — the `data-mf-focus` tag below is
  // what scopes them to this input. So they are all configurable, not hard-coded.
  finderInput.dataset.mfFocus = 'finder';
  normalToggle.addEventListener('click', () => void setNormalShown(!normalShown));
  normalFreeze.addEventListener('change', () => void setNormalFrozen(normalFreeze.checked));
  normalInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyQuery();
  });
  columnsInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyColumns();
  });
  byId(root, 'bulk-open').addEventListener('click', () => {
    void commands.invoke('metarecord-list:bulk-edit');
  });
  byId(root, 'bulk-apply').addEventListener('click', () => void applyBulkEdit());
  byId(root, 'bulk-cancel').addEventListener('click', () => bulkForm.classList.remove('open'));
  bulkName.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyBulkEdit();
  });

  // ── Wiring ──────────────────────────────────────────────────────────────

  commands.register('metarecord-list:next', {
    label: 'Metarecord list: move the selection down',
    handler: () => setCursor(cursorIndex + 1),
  });
  commands.register('metarecord-list:prev', {
    label: 'Metarecord list: move the selection up',
    handler: () => setCursor(cursorIndex - 1),
  });
  commands.register('metarecord-list:first', {
    label: 'Metarecord list: move the selection to the first row',
    handler: () => setCursor(0),
  });
  commands.register('metarecord-list:last', {
    label: 'Metarecord list: move the selection to the last loaded row',
    handler: () => setCursor(metarecords.length - 1),
  });
  commands.register('metarecord-list:page-next', {
    label: 'Metarecord list: load the next page (same as scrolling to the bottom)',
    handler: () => (nextCursor ? fetchPage(false) : undefined),
  });
  commands.register('metarecord-list:select-toggle', {
    label: 'Metarecord list: toggle multi-selection',
    handler: () => toggleChecked(),
  });
  commands.register('metarecord-list:select-none', {
    label: 'Metarecord list: clear the multi-selection',
    handler: () => clearChecked(),
  });
  commands.register('metarecord-list:open', {
    label: 'Metarecord list: open the selection in the other panel',
    handler: () => openSelected(),
  });
  commands.register('metarecord-list:set-mode', {
    label: 'Metarecord list: switch display mode (table | grid)',
    handler: (newMode) => {
      mode = newMode === 'grid' ? 'grid' : 'table';
      bodyEl.classList.toggle('grid', mode === 'grid');
    },
  });
  commands.register('metarecord-list:find', {
    label: 'Metarecord list: focus the finder (quick ordered-substring filter)',
    handler: () => finderInput.focus(),
  });
  commands.register('metarecord-list:apply-finder', {
    label: 'Metarecord list: re-run the finder filter now (bypass the debounce)',
    // The explicit re-run (Enter in the finder) also records the text in the
    // finder's input history; the debounced keystroke path does not.
    handler: () => applyFinder({ record: true }),
  });
  commands.register('metarecord-list:edit-query', {
    label: 'Metarecord list: focus the query input',
    handler: () => queryInput.focus(),
  });
  commands.register('metarecord-list:toggle-normal', {
    label: 'Metarecord list: show/hide the normal DSL editor',
    handler: () => setNormalShown(!normalShown),
  });
  commands.register('metarecord-list:edit-columns', {
    label: 'Metarecord list: focus the columns input',
    handler: () => columnsInput.focus(),
  });
  commands.register('metarecord-list:apply-query', {
    label: 'Metarecord list: apply the query',
    handler: () => applyQuery(),
  });
  commands.register('metarecord-list:apply-columns', {
    label: 'Metarecord list: apply the displayed columns',
    handler: () => applyColumns(),
  });
  commands.register('metarecord-list:refresh', {
    label: 'Metarecord list: reload from the daemon',
    handler: () => {
      queryRan = true; // refresh is also how the deferred initial load is run
      return fetchPage(true);
    },
  });
  commands.register('metarecord-list:bulk-edit', {
    label: 'Metarecord list: set/append/remove a field on every metarecord matching the query',
    reveal: true,
    handler: () => openBulkForm(),
  });
  commands.register('metarecord-list:set-page-size', {
    label: 'Metarecord list: set the page size (results per fetch)',
    handler: async (raw) => {
      const n = Math.floor(Number(raw));
      if (!Number.isFinite(n) || n < 1) throw new Error(`invalid page size: "${raw ?? ''}"`);
      await workspace.set('metarecord-list:page-size', n);
    },
  });

  // Keybindings for this panel live in keybindings.toml (when = "metarecord-list").

  let pickFocused = false; // focus the finder once when opened as a picker

  async function start() {
    const activeRepo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    if (activeRepo !== repo) {
      // A new repo: empty the list and disarm the query — it only runs again
      // on an explicit user action (see `queryRan`).
      repo = activeRepo;
      repoRoot = null;
      queryRan = false;
      metarecords = [];
      nextCursor = null;
      total = null;
      cursorIndex = -1;
      orphanCache = new Map();
      if (checked.size > 0) {
        checked = new Set();
        await workspace.set('selected_metarecords', []);
      }
    }
    byId(root, 'no-repo').hidden = repo !== null;
    if (!queryInitialized) {
      queryInitialized = true;
      await recomputeQuery();
    }
    if (repo !== null) {
      // Warm the field catalog so the finder can auto-detect osm/osmd per field.
      await cache.fetchFields(repo).catch(() => {});
      if (!pickFocused && (await workspace.get('pick_request'))) {
        pickFocused = true;
        finderInput.focus();
        queryRan = true; // a value picker needs rows to pick from
      }
    }
    if (queryRan) await fetchPage(true);
    else render(); // empty list + the "query not run" hint
  }

  // The first query waits for the first actual display.
  const deferredStart = () => void start();
  workspace.onChange('metarecords:dirty', () => metafolder.whenVisible(deferredStart));
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  workspace.onChange('metarecord-list:columns', async (value) => {
    setColumns(value);
    await reresolveColumns();
    render();
  });
  workspace.onChange('metarecord-list:page-size', (value) => {
    const next = sanitizePageSize(value);
    if (next === pageSize) return;
    pageSize = next;
    if (queryRan) void fetchPage(true);
  });
  workspace.onChange('metarecord-list:finder-fields', (value) => {
    finderFields = Array.isArray(value) && value.length ? value : DEFAULT_FINDER_FIELDS.slice();
    updateFinderFieldsLabel();
    if (queryRan) void fetchPage(true);
  });

  setColumns(await workspace.get('metarecord-list:columns'));
  widths = /** @type {Record<string, number>} */ (
    (await workspace.get('metarecord-list:column-widths')) ?? {}
  );
  pageSize = sanitizePageSize(await workspace.get('metarecord-list:page-size'));

  // Restore the finder (quick filter) state.
  finderText = String((await workspace.get('metarecord-list:finder')) ?? '');
  finderInput.value = finderText;
  const storedFinderFields = await workspace.get('metarecord-list:finder-fields');
  finderFields =
    Array.isArray(storedFinderFields) && storedFinderFields.length
      ? storedFinderFields
      : DEFAULT_FINDER_FIELDS.slice();
  updateFinderFieldsLabel();

  // Restore the two-zone query editor (values only — no daemon call here).
  queryInput.value = String((await workspace.get('metarecord-list:query')) ?? '');
  normalInput.value = String((await workspace.get('metarecord-list:normal-query')) ?? '');
  normalFrozen = (await workspace.get('metarecord-list:normal-frozen')) === true;
  normalFreeze.checked = normalFrozen;
  normalInput.readOnly = !normalFrozen;
  normalShown = (await workspace.get('metarecord-list:normal-shown')) === true;
  normalEditor.hidden = !normalShown;
  normalToggle.textContent = normalShown ? 'Hide normal DSL' : 'Show normal DSL';

  metafolder.whenVisible(deferredStart);

  return () => {
    clearTimeout(finderTimer);
    finderHistory.detach();
    queryHistory.detach();
    document.removeEventListener('mousemove', /** @type {EventListener} */ (onMouseMove));
    document.removeEventListener('mouseup', onMouseUp);
    detachScroll();
  };
}

/** The message of a thrown daemon/parser error. */
function messageOf(/** @type {unknown} */ error) {
  return error instanceof Error ? error.message : String(error);
}
