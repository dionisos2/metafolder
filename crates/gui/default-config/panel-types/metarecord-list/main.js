// metarecord-list panel: metarecords of the active repo filtered by an embedded
// DSL query; primary selection source (spec-gui "metarecord-list panel type").

import { byId, el, fields, qs, thumbnail } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { createPagedList } from '/__paged-list.js';
import { createTypePicker, widgetFor, bulkSetBody, MATCH_ALL, createPickRunner } from '/__value-widget.js';
import { createSelect } from '/__select.js';
import { splitTerms, finderTargets, finderClause, composeQuery } from '/__finder.js';
import { fileMenuItems, metarecordMenuItems } from '/__file-actions.js';
import { attachHistory } from '/__history.js';
import { latestOnly } from '/__coalesce.js';
import { BULK_OPERATIONS, bulkCommandFor } from './bulk-ops.js';
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
  // Orphan view (spec-file-tracking "Orphan scan"): when on, `queryIR` is a
  // `uuid_in` set from a daemon disk scan rather than the DSL editor; leaving it
  // (Exit, or applying/clearing a query) restores the editor-driven query.
  let orphanMode = false;
  /** @type {string[]} the uuids the last orphan scan returned */
  let orphanUuids = [];
  let finderText = ''; // quick OSM filter, AND-ed onto the base query
  /** @type {string[]} */
  let finderFields = DEFAULT_FINDER_FIELDS.slice();
  /** @type {ReturnType<typeof setTimeout>|undefined} */
  let finderTimer;
  let normalShown = false; // zone B (normal DSL) revealed?
  let normalFrozen = false; // zone B decoupled (hand-edited, authoritative)?
  let queryInitialized = false; // first query compiled on first display
  // The list runs its query eagerly on first display (and whenever a repo
  // becomes active): searches are fast enough that showing the repo's contents
  // straight away beats making the user press Enter/refresh first. Set in
  // `start()` once a repo is present; reset on repo change so the new repo is
  // re-run from scratch.
  let queryRan = false;
  /** The last fetch failure (daemon down/incompatible, query rejected), shown
   *  as a persistent body state so a failed load is never mistaken for an
   *  empty result. Cleared on the next successful fetch. @type {string|null} */
  let fetchError = null;
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
  const orphanBanner = byId(root, 'orphan-banner');
  const orphanCountEl = byId(root, 'orphan-count');
  const finderInput = byId(root, 'finder-input', HTMLInputElement);
  const finderFieldsLabel = byId(root, 'finder-fields');
  const queryInput = byId(root, 'query-input', HTMLInputElement);
  const columnsInput = byId(root, 'columns-input', HTMLInputElement);
  const queryError = byId(root, 'query-error');
  const columnsError = byId(root, 'columns-error');
  const normalToggle = byId(root, 'normal-toggle');
  const normalEditor = byId(root, 'normal-editor');
  const normalInput = byId(root, 'normal-input', HTMLInputElement);
  const normalError = byId(root, 'normal-error');
  const normalFreeze = byId(root, 'normal-freeze', HTMLInputElement);
  const bulkForm = byId(root, 'bulk-form');
  // `syncBulkOpUi` is a hoisted function declaration, so onChange can name it here.
  const bulkOp = createSelect(byId(root, 'bulk-op'), {
    value: 'set',
    options: [
      { value: 'set', label: 'Set (replace all rows)' },
      { value: 'append', label: 'Append (add a row)' },
      { value: 'remove', label: 'Remove (delete matching rows)' },
      { value: 'unset', label: 'Unset (remove the field)' },
      { value: 'delete', label: 'Delete metarecords' },
    ],
    onChange: () => syncBulkOpUi(),
  });
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

  // Re-resolve + re-render the displayed rows after the change feed reports
  // they changed daemon-side (e.g. the watcher reflected a GUI-initiated rename
  // ~500 ms later, so mfr_path — and thus the orphan state and any path-derived
  // column — is only now up to date). Non-disruptive: it keeps the loaded
  // pages, cursor and selection, only re-resolving the (now invalidated) tree
  // refs from the cache and repainting. Without this, a false "orphaned" marker
  // painted from the pre-rename path would linger until a manual refresh.
  async function refreshDisplayed() {
    if (metarecords.length === 0) return;
    await prepareNow(metarecords);
    render();
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
    // Resolved paths are leading-"/"-rooted ('' is the repo root itself), so the
    // absolute path is a plain concatenation with the repo root.
    return rel.map((p) => (p === '' ? rootPath : `${rootPath}${p}`));
  }

  // Whether the cache can currently answer for a metarecord's mfr_path
  // positions. False while a fresh resolution is pending (REFRESH): an
  // invalidation landing on an in-flight resolve makes the cache drop that
  // answer, and the gap is not evidence about the file.
  /** @param {Metafolder.Metarecord} metarecord */
  function pathsResolved(metarecord) {
    const r = repo;
    return !!r && cache.readTreeRef(r, 'mfr_path', metarecord.uuid) !== REFRESH;
  }

  // Whether a metarecord's mfr_type is a directory (picks the paste target for
  // the file-actions menu).
  /** @param {Metafolder.Metarecord} metarecord */
  function isDirMetarecord(metarecord) {
    const typeValue = fields(metarecord, 'mfr_type')[0]?.value;
    return typeValue?.type === 'string' && typeValue.value === 'dir';
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
      // The query actually run (base + finder clause). Published on every reset
      // so other panels (e.g. metarecord-detail's bulk field edits) can target
      // exactly what the list shows — the live finder narrowing included.
      const effQuery = effectiveQuery() ?? MATCH_ALL;
      if (reset) await workspace.set('metarecord-list:effective-query', effQuery);
      let result;
      try {
        result = await cache.query(r, {
          query: effQuery,
          select: '*',
          limit: pageSize,
          ...(reset && { count: true }), // daemon-side COUNT, no extra pages
          ...(sort.length > 0 && { sort }),
          ...(nextCursor && { cursor: nextCursor }),
        });
      } catch (error) {
        // A failed fetch must not read as "0 results": record it and render a
        // persistent error state (the status-bar flash alone vanishes).
        fetchError = error instanceof Error ? error.message : String(error);
        await statusBar.error(error);
        render();
        return;
      }
      fetchError = null; // a fresh page arrived; clear any stale error state
      // The page's metarecords come straight from the query result — not re-read
      // from the cache, which a concurrent change-feed invalidation may have left
      // unpopulated (that would drop every row and render an empty list).
      const fetched = /** @type {Metafolder.Metarecord[]} */ (result.records);
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
    // An mfr_path the cache has not resolved yet says nothing about the file.
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
            class: [
              'card',
              index === cursorIndex && 'cursor',
              checked.has(metarecord.uuid) && 'checked',
            ],
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
    // A fetch failure surfaces as a persistent body placeholder (only when the
    // list is otherwise empty, so a partial page keeps its rows) and always in
    // the status line — never a silently empty list.
    const errorEl = byId(root, 'fetch-error');
    const showErrorBody = fetchError !== null && metarecords.length === 0;
    errorEl.hidden = !showErrorBody;
    if (showErrorBody) {
      errorEl.textContent = `⚠ Could not load metarecords\n${fetchError}`;
    }
    statusLine.classList.toggle('error', fetchError !== null);
    statusLine.textContent = fetchError !== null
      ? `⚠ ${fetchError}`
      : !queryRan
        ? '' // no active repo: nothing to show yet
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
    // Applying an editor query leaves the orphan view (its uuid_in override).
    orphanMode = false;
    orphanBanner.hidden = true;
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

  /** Empties all three search fields and re-runs (empty query = match all). */
  async function clearAllQueries() {
    orphanMode = false;
    orphanBanner.hidden = true;
    finderInput.value = '';
    finderText = '';
    queryInput.value = '';
    normalInput.value = '';
    queryError.textContent = '';
    normalError.textContent = '';
    // Back to mirroring the (now empty) simplified query, so B shows empty too.
    if (normalFrozen) await setNormalFrozen(false);
    await recomputeQuery(); // queryIR = null (match all)
    await persistQueryState();
    await workspace.set('metarecord-list:finder', '');
    queryRan = true;
    await fetchPage(true);
  }

  // ── Orphan view (spec-file-tracking "Orphan scan") ────────────────────────

  /** Enter (or refresh) the orphan view: scan the disk and show the missing-file
   *  records via a `uuid_in` query. Exits to the normal view when none remain. */
  async function showOrphans() {
    const r = repo;
    if (!r) return;
    let resp;
    try {
      resp = /** @type {{orphans?: {uuid: string}[]}} */ (
        await daemon.call('POST', `/repos/${r}/orphans/scan`, {})
      );
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    orphanUuids = (resp.orphans ?? []).map((o) => o.uuid);
    if (orphanUuids.length === 0) {
      await statusBar.message('No orphans — every tracked file is present.', statusMessageMs);
      await exitOrphans();
      return;
    }
    orphanMode = true;
    queryIR = { type: 'uuid_in', uuids: orphanUuids };
    const n = orphanUuids.length;
    orphanCountEl.textContent = `${n} orphaned metarecord${n === 1 ? '' : 's'} — the tracked file is missing`;
    orphanBanner.hidden = false;
    queryRan = true;
    await fetchPage(true);
  }

  /** Leave the orphan view and restore the editor-driven query. */
  async function exitOrphans() {
    if (orphanMode) orphanMode = false;
    orphanBanner.hidden = true;
    await applyQuery();
  }

  /** Orphan the scanned records (mfr_path_old frozen, mfr_path → Nothing,
   *  cascading), after confirmation, then re-scan. */
  async function clearOrphans() {
    const r = repo;
    if (!r || !orphanMode || orphanUuids.length === 0) return;
    const n = orphanUuids.length;
    if (!confirm(`Orphan ${n} metarecord${n === 1 ? '' : 's'}? mfr_path becomes Nothing (its origin is kept in mfr_path_old). This can be rolled back.`)) {
      return;
    }
    try {
      const resp = /** @type {{cleared?: number}} */ (
        await daemon.call('POST', `/repos/${r}/orphans/clear`, { uuids: orphanUuids })
      );
      await statusBar.message(`Cleared ${resp.cleared ?? 0} orphan(s).`, statusMessageMs);
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    await workspace.set('metarecords:dirty', Date.now()); // nudge other panels
    await showOrphans(); // re-scan: shows any remainder, or exits if none
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
  // `noField` ops (delete) act on the matched metarecords themselves — no field
  // name, no value; the response counts `deleted` rather than `updated`.
  /** @type {Record<string, {path: string, verb: string, prep: string, valueless?: boolean, noField?: boolean}>} */
  const BULK_OPS = {
    set: { path: 'query/fields/set', verb: 'Set', prep: 'on' },
    append: { path: 'query/fields/append', verb: 'Append', prep: 'to' },
    remove: { path: 'query/fields/remove', verb: 'Remove', prep: 'from' },
    unset: { path: 'query/fields/unset', verb: 'Unset', prep: 'from', valueless: true },
    delete: { path: 'query/delete', verb: 'Delete', prep: '', valueless: true, noField: true },
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

  // Hide the type picker + value row for value-less ops (unset, delete), and
  // the field-name input + force checkbox for ops that take no field (delete).
  const bulkValueRow = byId(root, 'bulk-value-row');
  const bulkTypeBtn = byId(root, 'bulk-type');
  const bulkForceLabel = byId(root, 'bulk-force-label');
  function syncBulkOpUi() {
    const op = BULK_OPS[bulkOp.get() ?? 'set'] ?? BULK_OPS.set;
    const noField = op.noField === true;
    bulkValueRow.hidden = op.valueless === true;
    bulkTypeBtn.hidden = op.valueless === true || noField;
    bulkName.hidden = noField;
    bulkForceLabel.hidden = noField;
  }
  syncBulkOpUi();

  function openBulkForm() {
    bulkError.textContent = '';
    bulkForm.classList.add('open');
    syncBulkOpUi();
    if (!bulkName.hidden) bulkName.focus();
  }

  // Clicking "Edit / delete on query" again (or re-invoking the command) closes
  // the form when it is already open, rather than re-opening it.
  function toggleBulkForm() {
    if (bulkForm.classList.contains('open')) bulkForm.classList.remove('open');
    else openBulkForm();
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
      const op = BULK_OPS[bulkOp.get() ?? 'set'] ?? BULK_OPS.set;
      // Bulk actions target the effective (finder-filtered) set — you act on
      // what you see.
      const effQ = effectiveQuery();
      const n = await countMatches();

      if (op.noField) {
        // Delete the matched metarecords themselves. This removes the
        // metarecords; any associated files stay on disk (untracked). Confirm
        // with the count since it is not undoable from the UI.
        if (n === 0) {
          void statusBar.message('No metarecords match — nothing to delete.', statusMessageMs);
          return;
        }
        if (
          !confirm(
            `Delete ${n} metarecord${n === 1 ? '' : 's'}? This removes the metarecords ` +
              `(any files stay on disk).`,
          )
        )
          return;
        const resp = /** @type {{deleted?: number}} */ (
          await daemon.call('POST', `/repos/${repo}/${op.path}`, { query: effQ ?? MATCH_ALL })
        );
        const deleted = resp.deleted ?? 0;
        bulkForm.classList.remove('open');
        void statusBar.message(
          `Deleted ${deleted} metarecord${deleted === 1 ? '' : 's'}.`,
          statusMessageMs,
        );
        await workspace.set('metarecords:dirty', Date.now());
        return;
      }

      const name = bulkName.value.trim();
      if (!name) throw new Error('field name is required');
      const force = name.startsWith('mfr_') || bulkForce.checked;
      if (!confirm(`${op.verb} "${name}" ${op.prep} ${n} metarecord${n === 1 ? '' : 's'}?`)) return;
      // Value-less ops (unset) act on the name alone.
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
  // Enter applies AND leaves the field (blur) so the panel accelerators resume;
  // Shift+Enter applies but keeps the focus for another edit.
  queryInput.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    void applyQuery();
    if (!event.shiftKey) queryInput.blur();
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
    if (event.key !== 'Enter') return;
    void applyQuery();
    if (!event.shiftKey) normalInput.blur();
  });
  columnsInput.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    void applyColumns();
    if (!event.shiftKey) columnsInput.blur();
  });
  byId(root, 'orphans-btn').addEventListener('click', () => void showOrphans());
  byId(root, 'orphan-clear').addEventListener('click', () => void clearOrphans());
  byId(root, 'orphan-exit').addEventListener('click', () => void exitOrphans());
  byId(root, 'bulk-open').addEventListener('click', () => {
    void commands.invoke('metarecord-list:open-bulk-edit');
  });
  byId(root, 'bulk-apply').addEventListener('click', () => void applyBulkEdit());
  byId(root, 'bulk-cancel').addEventListener('click', () => bulkForm.classList.remove('open'));
  bulkName.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyBulkEdit();
  });

  // ── Wiring ──────────────────────────────────────────────────────────────

  void commands.register('metarecord-list:next', {
    label: 'Metarecord list: move the selection down',
    handler: () => setCursor(cursorIndex + 1),
  });
  void commands.register('metarecord-list:prev', {
    label: 'Metarecord list: move the selection up',
    handler: () => setCursor(cursorIndex - 1),
  });
  void commands.register('metarecord-list:first', {
    label: 'Metarecord list: move the selection to the first row',
    handler: () => setCursor(0),
  });
  void commands.register('metarecord-list:last', {
    label: 'Metarecord list: move the selection to the last loaded row',
    handler: () => setCursor(metarecords.length - 1),
  });
  void commands.register('metarecord-list:page-next', {
    label: 'Metarecord list: load the next page (same as scrolling to the bottom)',
    handler: () => (nextCursor ? fetchPage(false) : undefined),
  });
  void commands.register('metarecord-list:select-toggle', {
    label: 'Metarecord list: toggle multi-selection',
    handler: () => toggleChecked(),
  });
  void commands.register('metarecord-list:select-none', {
    label: 'Metarecord list: clear the multi-selection',
    handler: () => clearChecked(),
  });
  void commands.register('metarecord-list:open', {
    label: 'Metarecord list: open the selection in the other panel',
    handler: () => openSelected(),
  });
  void commands.register('metarecord-list:set-mode', {
    label: 'Metarecord list: switch display mode (table | grid)',
    handler: (newMode) => {
      mode = newMode === 'grid' ? 'grid' : 'table';
      bodyEl.classList.toggle('grid', mode === 'grid');
    },
  });
  void commands.register('metarecord-list:focus-finder', {
    label: 'Metarecord list: focus the finder (quick ordered-substring filter)',
    handler: () => finderInput.focus(),
  });
  void commands.register('metarecord-list:apply-finder', {
    label: 'Metarecord list: re-run the finder filter now (bypass the debounce)',
    // The explicit re-run (Enter in the finder) also records the text in the
    // finder's input history; the debounced keystroke path does not.
    handler: () => applyFinder({ record: true }),
  });
  void commands.register('metarecord-list:focus-query', {
    label: 'Metarecord list: focus the query input',
    handler: () => queryInput.focus(),
  });
  void commands.register('metarecord-list:toggle-normal', {
    label: 'Metarecord list: show/hide the normal DSL editor',
    handler: () => setNormalShown(!normalShown),
  });
  void commands.register('metarecord-list:focus-columns', {
    label: 'Metarecord list: focus the columns input',
    handler: () => columnsInput.focus(),
  });
  // Open the normal-DSL editor, freeze it (so it drives the query and is
  // editable), and focus it for hand-editing — the counterpart of focus-finder
  // / focus-query for the third search field.
  void commands.register('metarecord-list:edit-normal', {
    label: 'Metarecord list: open, freeze and focus the normal DSL editor',
    handler: async () => {
      await setNormalShown(true);
      await setNormalFrozen(true);
      normalInput.focus();
    },
  });
  // Focus the simplified query field for hand-editing — and unfreeze the normal
  // DSL editor first: while B is frozen it is authoritative, so typing in A
  // would have no effect on the query (spec-gui "Query editor"). Unfreezing
  // discards any manual edit to B, which re-mirrors expand(A).
  void commands.register('metarecord-list:edit-simplified', {
    label: 'Metarecord list: unfreeze the normal DSL editor and focus the simplified query field',
    handler: async () => {
      await setNormalFrozen(false);
      queryInput.focus();
    },
  });
  // Clear all three search fields (finder + simplified + normal) and re-run —
  // an empty query matches everything, so this resets to the full repo.
  void commands.register('metarecord-list:clear-queries', {
    label: 'Metarecord list: clear the finder, simplified and normal query fields',
    handler: () => clearAllQueries(),
  });
  void commands.register('metarecord-list:orphans', {
    label: 'Metarecord list: show orphaned metarecords (tracked file missing)',
    handler: () => showOrphans(),
  });
  // Clear-then-edit, one field at a time. The finder is a live filter, so
  // clearing it re-runs immediately (widening the result); the DSL fields wait
  // for an explicit Enter, matching their normal type-then-apply flow.
  void commands.register('metarecord-list:clear-edit-finder', {
    label: 'Metarecord list: clear the finder and focus it',
    handler: async () => {
      finderInput.value = '';
      finderInput.focus();
      await applyFinder();
    },
  });
  void commands.register('metarecord-list:clear-edit-simplified', {
    label: 'Metarecord list: clear the simplified query field, unfreeze zone B and focus it',
    handler: async () => {
      queryInput.value = '';
      queryError.textContent = '';
      // Same reason as `edit-simplified`: a frozen B would ignore the field we
      // just handed the focus to.
      await setNormalFrozen(false);
      queryInput.focus();
    },
  });
  void commands.register('metarecord-list:clear-edit-normal', {
    label: 'Metarecord list: open the normal DSL editor, clear it, freeze and focus it',
    handler: async () => {
      await setNormalShown(true);
      await setNormalFrozen(true);
      normalInput.value = '';
      normalError.textContent = '';
      normalInput.focus();
    },
  });
  // Enter in the finder: re-run the filter AND leave the field (blur), so the
  // panel accelerators resume without a separate Escape. `apply-finder` (bound
  // to Shift+Enter) is the stay-focused variant.
  void commands.register('metarecord-list:submit-finder', {
    label: 'Metarecord list: re-run the finder filter and leave the field',
    handler: async () => {
      await applyFinder({ record: true });
      finderInput.blur();
    },
  });
  void commands.register('metarecord-list:apply-query', {
    label: 'Metarecord list: apply the query',
    handler: () => applyQuery(),
  });
  void commands.register('metarecord-list:apply-columns', {
    label: 'Metarecord list: apply the displayed columns',
    handler: () => applyColumns(),
  });
  void commands.register('metarecord-list:refresh', {
    label: 'Metarecord list: reload from the daemon',
    handler: () => {
      queryRan = true; // a manual refresh always loads, even with no repo yet armed
      return fetchPage(true);
    },
  });
  // Two entry points to bulk editing (spec-gui "metarecord-list panel type"):
  //  · `open-bulk-edit` — mouse-oriented: the in-panel form (op picker + value
  //    widget), opened by the footer button.
  //  · `bulk-edit` — keyboard-oriented: collects the operation in the command
  //    input (completion), then delegates to the per-operation completion
  //    command, which collects its own field/value. Bound to `m b`.
  void commands.register('metarecord-list:open-bulk-edit', {
    label: 'Metarecord list: open the bulk edit / delete form (set/append/remove/unset/delete)',
    reveal: true,
    handler: () => toggleBulkForm(),
  });
  void commands.register('metarecord-list:bulk-edit', {
    label: 'Metarecord list: bulk edit / delete on the current query (pick an operation)',
    args: [
      {
        name: 'operation',
        prompt: () => 'Operation? (set / append / remove / unset / delete)',
        complete: () => BULK_OPERATIONS,
      },
    ],
    handler: (operation) => commands.invoke(bulkCommandFor(operation)),
  });
  void commands.register('metarecord-list:set-page-size', {
    label: 'Metarecord list: set the page size (results per fetch)',
    handler: async (raw) => {
      const n = Math.floor(Number(raw));
      if (!Number.isFinite(n) || n < 1) throw new Error(`invalid page size: "${raw ?? ''}"`);
      await workspace.set('metarecord-list:page-size', n);
    },
  });

  // Keybindings for this panel live in keybindings.toml (when = "metarecord-list").

  let picking = false; // true while this list is open as a value picker

  /** The `metarecords` index of the row/card under a context-menu event, or
   *  -1 when the click missed a row (header, empty space). The event is handled
   *  at the shell `window`, where `event.target` is retargeted to the panel's
   *  Shadow-DOM host — so the real clicked node is found through
   *  `composedPath()`, which still crosses the boundary.
   *  @param {MouseEvent} event */
  function rowIndexFromEvent(event) {
    for (const node of event.composedPath()) {
      if (!(node instanceof Element)) continue;
      if (node.matches('tr.row')) return [...rows.children].indexOf(node);
      if (node.matches('.card')) return [...grid.children].indexOf(node);
    }
    return -1;
  }

  // Right-click menu: acts on the row under the pointer. The clicked row
  // becomes the cursor first, so every action targets it rather than whatever
  // the keyboard cursor last sat on — `metarecord:trash`/`pick:confirm` read
  // `selected_metarecord`, which the cursor keeps in sync. All actions confirm
  // where they mutate anything.
  metafolder.contextMenu.addDefaultItems((event) => {
    const index = rowIndexFromEvent(event);
    if (index >= 0 && index !== cursorIndex) void setCursor(index);
    const target = metarecords[index >= 0 ? index : cursorIndex];
    if (!target) return [];
    const paths = pathsOf(target);
    // The shared "Metarecord" section (open in detail/file, reveal folder, Copy
    // UUID) — every metarecord-bearing panel shows the same one. A value picker
    // prepends "Pick this metarecord" right after the header.
    /** @type {Metafolder.MenuItem[]} */
    const items = metarecordMenuItems({
      metafolder,
      uuid: target.uuid,
      hasFile: paths.length > 0,
      leading: picking
        ? [{ label: 'Pick this metarecord', action: () => void commands.invoke('pick:confirm') }]
        : [],
    });
    if (repo && paths.length > 0) {
      // Cut / Copy / Paste / Rename / Duplicate / Move-to-trash on the file
      // (shared with the file manager — see /__file-actions.js).
      items.push(
        '-',
        ...fileMenuItems({
          metafolder,
          repo,
          path: paths[0],
          isDir: isDirMetarecord(target),
          onChanged: () => void workspace.set('metarecords:dirty', Date.now()),
        }),
      );
    }
    return items;
  });

  let pickFocused = false; // focus the finder once when opened as a picker

  async function start() {
    const activeRepo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    if (activeRepo !== repo) {
      // A new repo: empty the list and disarm the query; it is re-armed below
      // so the new repo's contents load on this same display (see `queryRan`).
      repo = activeRepo;
      repoRoot = null;
      queryRan = false;
      fetchError = null;
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
      // Run eagerly: show the repo's contents on first display rather than
      // waiting for an explicit apply/refresh (searches are fast enough).
      queryRan = true;
      const pickRequest = await workspace.get('pick_request');
      picking = !!pickRequest; // arms the "Pick this metarecord" context item
      if (!pickFocused && pickRequest) {
        pickFocused = true;
        finderInput.focus();
      }
    }
    if (queryRan) await fetchPage(true);
    else render(); // no repo: empty list
  }

  // The first query waits for the first actual display.
  const deferredStart = () => void start();
  // A `metarecords:dirty` nudge fires right after a local mutation (e.g. a
  // rename's fs.move), but a watcher-driven daemon write only lands after its
  // ~500 ms quiet period — after our immediate refresh has already read the
  // stale path. Poll the change feed a couple of times over the next seconds so
  // the settled change is picked up promptly (the subscription above then
  // repaints), rather than waiting for the slow background poll. The 7 s timer
  // remains the backstop.
  /** @type {ReturnType<typeof setTimeout>[]} */
  let catchupTimers = [];
  function scheduleCatchupSync() {
    for (const t of catchupTimers) clearTimeout(t); // supersede the previous nudge
    catchupTimers = [700, 1800].map((delay) =>
      setTimeout(() => void (repo && cache.sync(repo)), delay),
    );
  }
  workspace.onChange('metarecords:dirty', () => {
    metafolder.whenVisible(deferredStart);
    scheduleCatchupSync();
  });
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));

  // Keep the list live when the daemon reflects a change out-of-band from our
  // own query round-trip — chiefly a watcher-driven update (a GUI rename lands
  // in the daemon ~500 ms after the fs.move, well after our immediate
  // `metarecords:dirty` refresh already read the stale path). The background
  // change-feed poll invalidates the cache; here we react to it so the affected
  // rows are re-resolved and repainted (clearing any stale orphan marker).
  const unsubscribeCache = cache.subscribe(({ repo: changedRepo, uuids }) => {
    if (changedRepo !== repo || metarecords.length === 0) return;
    if (uuids === null) {
      orphanCache = new Map(); // coarse refresh: the whole repo may have changed
    } else {
      const displayed = new Set(metarecords.map((m) => m.uuid));
      if (!uuids.some((u) => displayed.has(u))) return; // nothing on screen changed
      for (const u of uuids) orphanCache.delete(u);
    }
    metafolder.whenVisible(() => void refreshDisplayed());
  });
  /** @param {unknown} value */
  async function onColumnsChanged(value) {
    setColumns(value);
    await reresolveColumns();
    render();
  }
  workspace.onChange('metarecord-list:columns', (value) => void onColumnsChanged(value));
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
  finderText = asText(await workspace.get('metarecord-list:finder'));
  finderInput.value = finderText;
  const storedFinderFields = await workspace.get('metarecord-list:finder-fields');
  finderFields =
    Array.isArray(storedFinderFields) && storedFinderFields.length
      ? storedFinderFields
      : DEFAULT_FINDER_FIELDS.slice();
  updateFinderFieldsLabel();

  // Restore the two-zone query editor (values only — no daemon call here).
  queryInput.value = asText(await workspace.get('metarecord-list:query'));
  normalInput.value = asText(await workspace.get('metarecord-list:normal-query'));
  normalFrozen = (await workspace.get('metarecord-list:normal-frozen')) === true;
  normalFreeze.checked = normalFrozen;
  normalInput.readOnly = !normalFrozen;
  normalShown = (await workspace.get('metarecord-list:normal-shown')) === true;
  normalEditor.hidden = !normalShown;
  normalToggle.textContent = normalShown ? 'Hide normal DSL' : 'Show normal DSL';

  metafolder.whenVisible(deferredStart);

  return () => {
    clearTimeout(finderTimer);
    for (const t of catchupTimers) clearTimeout(t);
    finderHistory.detach();
    queryHistory.detach();
    unsubscribeCache();
    document.removeEventListener('mousemove', /** @type {EventListener} */ (onMouseMove));
    document.removeEventListener('mouseup', onMouseUp);
    detachScroll();
  };
}

/** A persisted workspace variable read back as text: anything that is not a
 *  string (a stale value of another shape) reads as empty, never as
 *  "[object Object]".
 *  @param {unknown} value */
function asText(value) {
  return typeof value === 'string' ? value : '';
}

/** The message of a thrown daemon/parser error. */
function messageOf(/** @type {unknown} */ error) {
  return error instanceof Error ? error.message : String(error);
}
