// metarecord-list panel: metarecords of the active repo filtered by an embedded
// DSL query; primary selection source (doc "metarecord-list panel").

import { byId, el, fields, messageOf, qs, thumbnail, viewerButtons } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { createMultiSelect } from '/__multi-select.js';
import { MATCH_ALL } from '/__value-widget.js';
import {
  splitTerms,
  finderTargets,
  finderClause,
  finderClauseText,
  composeQuery,
  composeQueryText,
} from '/__finder.js';
import { fileMenuItems, metarecordMenuItems } from '/__file-actions.js';
import { attachHistory } from '/__history.js';
import { createListCursor } from '/__list-cursor.js';
import { createCatchup } from '/__watcher-settle.js';
import { createBulkEdit } from './bulk-edit.js';
import { createDisplayData } from './display-data.js';
import { asText, createQueryEditor } from './query-editor.js';
import { registerZoneCommands } from './zone-commands.js';
import { MOVES, createOrderMode, positionOf } from './order-mode.js';
import {
  parseColumns,
  isSortable,
  cellQuickText,
  cellText,
} from '/__columns.js';

// Smallest page of the three list panels: each row needs several daemon
// round-trips (TreeRef path resolution, ref-target metarecords) and parsing,
// so a modest page keeps a large result responsive on first display. The
// effective default comes from the GUI config (`[page-size].metarecord-list`);
// this is only the fallback, and the per-workspace page-size variable still
// overrides it.
const DEFAULT_PAGE_SIZE_FALLBACK = 100;
// Initial table columns. The effective value comes from the GUI config
// (`[panel-defaults.metarecord-list].columns`), overridden in turn by the
// per-workspace `metarecord-list:columns` variable; this is only the fallback.
const DEFAULT_COLUMNS = '(mfr_path:path|path:path) (mfr_type|mf_schema) mfr_mtime order_file';
// Fields the finder (quick OSM filter) searches by default, each with an
// explicit aspect (`field:path` for the tree_ref path, `field:value` for a plain
// value) so it never depends on the async field catalog. A bare `field` (no
// mode) auto-detects from the catalog. Missing fields contribute nothing.
// Configured by `[panel-defaults.metarecord-list].finder-fields`, overridable
// per workspace via `metarecord-list:finder-fields`.
const DEFAULT_FINDER_FIELDS = ['mfr_path:path', 'label:value', 'name:value'];
// Idle delay before the finder re-runs the query, so a burst of typing sends
// one request rather than one per keystroke.
const FINDER_DEBOUNCE_MS = 500;
// The column shown as a tile's name in grid mode
// (`[panel-defaults.metarecord-list].grid-name-column`).
const DEFAULT_GRID_NAME_COLUMN = 'mfr_path:path';

/**
 * Parses a columns spec, falling back to `fallback` when it is missing, empty
 * or invalid — a bad configured value must never break the panel.
 * @param {string|undefined} spec @param {string} fallback @returns {Column[]}
 */
function parseColumnsOr(spec, fallback) {
  try {
    const parsed = parseColumns(spec ?? '');
    if (parsed.length > 0) return parsed;
  } catch {
    /* invalid spec (stale config or persisted value): fall back */
  }
  return parseColumns(fallback);
}

/**
 * A column spec, as ./columns.js parses it.
 * @typedef {import('/__columns.js').Column} Column
 *
 * A sort key, as the daemon's query body takes it.
 * @typedef {{field: string, order: 'asc'|'desc'}} SortKey
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, bench, changes } = metafolder;
  const defaultPageSize = metafolder.pageSize ?? DEFAULT_PAGE_SIZE_FALLBACK;
  // UX timing knobs (config.toml `[panels]`), with the module fallbacks below.
  const { settings } = metafolder;
  const finderDebounceMs = settings.finderDebounceMs ?? FINDER_DEBOUNCE_MS;
  // Initial values from the GUI config (`[panel-defaults.metarecord-list]`),
  // with the module constants as fallbacks. A value persisted per workspace
  // (columns, finder fields) still wins over both.
  const defaultColumns = metafolder.defaults.columns ?? DEFAULT_COLUMNS;
  const defaultFinderFields = metafolder.defaults.finderFields ?? DEFAULT_FINDER_FIELDS;
  const gridNameColumn = parseColumnsOr(
    metafolder.defaults.gridNameColumn,
    DEFAULT_GRID_NAME_COLUMN,
  )[0];
  const statusMessageMs = settings.statusMessageMs ?? 5000;

  /** @type {string|null} */
  let repo = null;
  /** @type {Column[]} persisted per workspace (spec strings) */
  let columns = parseColumnsOr(defaultColumns, DEFAULT_COLUMNS);
  /** @type {Record<string, number>} column spec -> px; persisted per workspace */
  let widths = {};
  /** @type {Metafolder.Metarecord[]} */
  let metarecords = [];
  /** @type {string|null} */
  let nextCursor = null;
  /** @type {number|null} full result count (daemon-side COUNT, first page only) */
  let total = null;
  /** Bumped per count asked for: a count answers only if it is still the last. */
  let countSeq = 0;
  /** Aborts the count in flight: nobody reads it once the query changed, and
   *  the daemon cancels it rather than finishing it for nobody. */
  /** @type {AbortController|null} */
  let countAbort = null;

  /**
   * The number of metarecords `query` matches, into the footer's "/total" —
   * asked after the first page (doc "Sorting and postings").
   * Unknown until it arrives, and left unknown if it fails: the rows are
   * what the list is for.
   * @param {string} r @param {unknown} query
   */
  async function fetchCount(r, query) {
    const seq = ++countSeq;
    countAbort?.abort();
    const controller = new AbortController();
    countAbort = controller;
    try {
      const result = /** @type {{total?: number|null}} */ (
        await daemon.call(
          'POST',
          `/repos/${r}/query`,
          { query, select: '*', limit: 1, count: true },
          { signal: controller.signal },
        )
      );
      if (seq !== countSeq) return;
      total = result.total ?? null;
      render();
    } catch {
      // Stays unknown.
    }
  }
  let pageSize = defaultPageSize; // persisted per workspace
  let loading = false;
  let finderText = ''; // quick OSM filter, AND-ed onto the base query
  /** @type {string[]} */
  let finderFields = defaultFinderFields.slice();
  /** @type {ReturnType<typeof setTimeout>|undefined} */
  let finderTimer;
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
  /** @type {SortKey[]} */
  let sort = [];
  /** The int field the list is ordered by (doc "Ordering a list"), or null:
   *  while set, the sort is that field ascending and the headers do not sort.
   *  @type {string|null} */
  let orderField = null;
  let cursorIndex = -1;
  // The checked multi-selection (workspace `selected_metarecords`, doc
  // "Cross-panel selection"): workspace-wide and shared with the file manager,
  // kept across list changes — checking rows in several lists is what it is
  // for. The rows shown here are only where it is seen and edited.
  const selection = createMultiSelect({ workspace, daemon, render });
  let mode = 'table';

  const bodyEl = qs(root, '.mf-panel-body');
  const rows = byId(root, 'rows');
  const grid = byId(root, 'grid');
  const scroll = byId(root, 'scroll');
  const statusLine = byId(root, 'status-line');
  const finderInput = byId(root, 'finder-input', HTMLInputElement);
  const finderFieldsLabel = byId(root, 'finder-fields');
  const queryInput = byId(root, 'query-input', HTMLInputElement);
  const columnsInput = byId(root, 'columns-input', HTMLInputElement);
  const columnsError = byId(root, 'columns-error');

  // Per-repo input history (doc "Input history"): ctrl-p/ctrl-n walk,
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

  // ── Query (two-zone editor, ./query-editor.js) ──────────────────────────
  const editor = createQueryEditor({
    root,
    metafolder,
    history: queryHistory,
    onApply: () => {
      queryRan = true;
    },
    run: () => fetchPage(true),
  });

  // ── Display data (./display-data.js) ─────────────────────────────────
  const display = createDisplayData({
    daemon,
    bench,
    // `fs.exists`, not `fs.stat`: the latter follows a symlink, so a broken
    // one read as gone and coloured a file that is right there as an orphan.
    exists: (path) => metafolder.fs.exists(path),
    getRepo: () => repo,
    getColumns: () => shownColumns(),
    render,
  });
  const pathsOf = display.pathsOf;

  /** Field name → its value type, for the finder's per-field mode. Re-read on
   *  every fresh query (one cheap daemon call). */
  /** @type {Map<string, string>} */
  let fieldTypes = new Map();
  async function readFieldTypes() {
    const r = repo;
    if (!r) return;
    try {
      fieldTypes = new Map((await daemon.fields(r)).map((f) => [f.name, f.type]));
    } catch {
      // Unknown types only cost the finder its per-field mode detection.
    }
  }

  // Re-read + re-render the displayed rows' display data after the change feed
  // reports a change daemon-side (e.g. the watcher recorded a GUI-initiated
  // rename a quiet period later, so mfr_path — and thus the orphan state and
  // any path-derived column — is only now up to date). Non-disruptive: it keeps
  // the loaded pages, cursor and selection, only re-reading the tree paths and
  // repainting. Without this, a false "orphaned" marker painted from the
  // pre-rename path would linger until a manual refresh.
  async function refreshDisplayed() {
    if (metarecords.length === 0) return;
    await display.refresh(metarecords);
    render();
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
    await display.refresh(metarecords);
  }

  // The query actually run: the structural base query AND the finder's OSM
  // clause (mode auto-detected per field from the catalog). null = match all.
  function effectiveQuery() {
    const r = repo;
    const targets = finderTargets(finderFields, (f) => (r ? (fieldTypes.get(f) ?? null) : null));
    return composeQuery(editor.ir(), finderClause(splitTerms(finderText), targets));
  }

  // The same query as DSL text. Built from the same inputs by the same-shaped
  // builders, which the shared vectors keep honest (panel-shim/
  // finder-vectors.json). null = match all.
  function effectiveQueryText() {
    const r = repo;
    const targets = finderTargets(finderFields, (f) => (r ? (fieldTypes.get(f) ?? null) : null));
    return composeQueryText(editor.text(), finderClauseText(splitTerms(finderText), targets));
  }

  /** Aborts the page read in flight. @type {AbortController|null} */
  let pageAbort = null;
  /** A reset asked for while a fetch was in flight: run once it unwinds. */
  let resetPending = false;

  // Fetches are serialized on `loading`. A reset asked for meanwhile means the
  // query changed: what is in flight is aborted — the page and the count, which
  // the daemon then cancels instead of finishing for nobody — and the reset
  // runs once it has unwound, on the query as it is then. Returns false when
  // the call did not run now (no repo, or another fetch in flight).
  /** @param {boolean} reset @returns {Promise<boolean|undefined>} */
  async function fetchPage(reset) {
    const r = repo;
    if (!r) return false;
    if (loading) {
      if (reset) {
        resetPending = true;
        pageAbort?.abort();
        countAbort?.abort();
      }
      return false;
    }
    loading = true;
    const controller = new AbortController();
    pageAbort = controller;
    try {
      if (reset) {
        // The finder picks each field's match mode from its type: read the
        // catalogue fresh (a field may have been added or retyped).
        await readFieldTypes();
        // The checked selection outlives the list it was gathered in, so
        // nothing is dropped here for merely not matching the query — but a
        // metarecord that no longer exists is (it could never be shown or
        // unchecked again).
        await selection.pruneVanished();
      }
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
        total = null;
        countSeq += 1; // a count still on its way belongs to the old query
        countAbort?.abort();
        display.clear();
        void display.refreshMounts(); // a volume may have been plugged in or pulled out
      }
      // The query actually run (base + finder clause). Published on every reset
      // so other panels (e.g. metarecord-detail's bulk field edits) can target
      // exactly what the list shows — the live finder narrowing included.
      const effQuery = effectiveQuery() ?? MATCH_ALL;
      if (reset) {
        await workspace.set('metarecord-list:effective-query', effQuery);
        // ...and the same query as DSL text, for a script asking what the panel
        // is showing (`mf gui query`). '' = match all, the text counterpart of
        // MATCH_ALL.
        await workspace.set('metarecord-list:effective-query-text', effectiveQueryText() ?? '');
        // ...and its order, so whoever plays what the list shows plays it in
        // this order (the file panel's slideshow, doc "Slideshow").
        await workspace.set('metarecord-list:sort', sort);
      }
      let result;
      try {
        result = await daemon.query(r, {
          query: effQuery,
          select: '*',
          limit: pageSize,
          ...(sort.length > 0 && { sort }),
          ...(nextCursor && { cursor: nextCursor }),
        }, { signal: controller.signal });
      } catch (error) {
        // Replaced by a newer query: not a failure, and not this call's to show.
        if (controller.signal.aborted) return false;
        // A failed fetch must not read as "0 results": record it and render a
        // persistent error state (the status-bar flash alone vanishes).
        fetchError = error instanceof Error ? error.message : String(error);
        await statusBar.error(error);
        render();
        return;
      }
      fetchError = null; // a fresh page arrived; clear any stale error state
      const fetched = /** @type {Metafolder.Metarecord[]} */ (result.records);
      metarecords = metarecords.concat(fetched);
      nextCursor = result.nextCursor;
      await display.prepare(fetched); // pre-resolve display data; rendering stays sync
      // The total is asked for apart, once the page is in and prepared: a page
      // the daemon stops at its end would otherwise wait for every match to be
      // found — and a render landing mid-preparation would judge rows whose
      // display data is not there yet.
      if (reset) void fetchCount(r, effQuery);
      if (reset) {
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
      if (pageAbort === controller) pageAbort = null;
      if (resetPending) {
        resetPending = false;
        void fetchPage(true);
      }
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
      el('th', { class: 'open-col' }), // the rows' open buttons
      ...shownColumns().map((column) => {
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
              selection.has(metarecord.uuid) && 'checked',
            ],
            onclick: () => setCursor(index),
          },
          el('td', { class: 'open-cell' }, rowOpenButtons(index)),
          shownColumns().map((column) => el('td', {}, cellText(column, metarecord))),
        );
        display.mark(tr, metarecord);
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
              selection.has(metarecord.uuid) && 'checked',
            ],
            onclick: () => setCursor(index),
          },
          thumbnail(metafolder.guiServer, pathsOf(metarecord)[0], {
            glyphClass: 'glyph',
            token: metafolder.sessionToken,
          }),
          el(
            'div',
            { class: 'card-foot' },
            rowOpenButtons(index),
            el(
              'div',
              { class: 'name' },
              cellQuickText(gridNameColumn, metarecord) || metarecord.uuid.slice(0, 8),
            ),
          ),
        );
        display.mark(card, metarecord);
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
    // The selection count is the workspace-wide one (checks gathered in other
    // lists included); the parenthetical says how many of them are on screen.
    const selectedShown = metarecords.reduce((n, m) => n + (selection.has(m.uuid) ? 1 : 0), 0);
    const selectedTotal = selection.count();
    statusLine.textContent = fetchError !== null
      ? `⚠ ${fetchError}`
      : !queryRan
        ? '' // no active repo: nothing to show yet
        : `${metarecords.length}${total !== null ? `/${total}` : ''} metarecord${
            (total ?? metarecords.length) === 1 ? '' : 's'
          }` +
          (nextCursor ? ' (more available — scroll down)' : '') +
          (selectedTotal > 0
            ? ` — ${selectedTotal} selected` +
              (selectedShown < selectedTotal ? ` (${selectedShown} in this list)` : '')
            : '');
  }

  // ── Selection (workspace variables) ─────────────────────────────────────

  // The selections this list wrote whose echo has not come back yet (uuid →
  // how many). Every write comes back to every listener, the writer's
  // included, and possibly after the cursor has moved on: an echo is not
  // another panel's choice to follow.
  /** @type {Map<string, number>} */
  const pendingEchoes = new Map();

  // The table rows and the grid cards are index-aligned with `metarecords`:
  // moving the cursor retargets the highlight, and the selection is published
  // coalesced (doc "Moving the cursor").
  const cursor = createListCursor({
    containers: () => [rows, grid],
    publish: async () => {
      const metarecord = metarecords[cursorIndex];
      if (!metarecord) return;
      pendingEchoes.set(metarecord.uuid, (pendingEchoes.get(metarecord.uuid) ?? 0) + 1);
      await workspace.set('selected_metarecord', { uuid: metarecord.uuid, repo });
      await workspace.set('selected_paths', pathsOf(metarecord));
    },
  });

  /** @param {number} index */
  async function setCursor(index) {
    cursorIndex = Math.max(0, Math.min(index, metarecords.length - 1));
    if (!metarecords[cursorIndex]) {
      render();
      return;
    }
    await cursor.move(cursorIndex);
  }

  async function toggleChecked() {
    const metarecord = metarecords[cursorIndex];
    if (!metarecord) return;
    await selection.toggle(metarecord.uuid);
  }

  /** Check every loaded row, on top of whatever is already checked: the
   *  selection is gathered across lists, so "all" means "all of this list" and
   *  never drops an earlier check. The bulk commands then act on an explicit
   *  set rather than on the query, which is what makes the confirmation
   *  exact. */
  async function checkAll() {
    await selection.add(metarecords.map((e) => e.uuid));
  }

  async function clearChecked() {
    await selection.clear();
  }

  /** A row's (or card's) open buttons (doc "Opening a row"): each selects the
   *  row, then shows it in the other slot — `↗` in the file panel, greyed when
   *  the metarecord has no file, `ⓘ` in the detail panel. The keys do the same
   *  through `panel:open`: the row is the published selection already.
   *  @param {number} index */
  function rowOpenButtons(index) {
    return viewerButtons({
      hasFile: display.hasFile(metarecords[index]),
      hasMetarecord: true,
      open: async (what) => {
        await setCursor(index);
        await commands.invoke(`panel:open other ${what}`);
      },
    });
  }

  /** Empties all three search fields and re-runs (empty query = match all). */
  async function clearAllQueries() {
    finderInput.value = '';
    finderText = '';
    await editor.clear();
    await workspace.set('metarecord-list:finder', '');
    queryRan = true;
    await fetchPage(true);
  }

  // ── Orphans (doc "Finding and clearing orphans") ────────────────────────

  /** Show the metarecords `orphan:detect` marked. No scan, no hidden override:
   *  the marker is an ordinary field, so this is the query `orphan = true`
   *  typed into the visible DSL zone — which is exactly the point of the
   *  command, showing a new user what the detection wrote and where to edit it. */
  async function showOrphans() {
    await editor.show('orphan = true');
  }

  // ── Duplicates (doc "duplicates panel") ────────────────────────────────────

  /** Show what else is byte-identical to the selected metarecord. The point of
   *  the group model is reaching a file's twins without the caller ever
   *  handling a hash, so this is just `same(...)` typed into the DSL zone —
   *  visible, editable and composable. */
  async function showDuplicates() {
    const selected = /** @type {{uuid?: string} | null} */ (
      (await workspace.get('selected_metarecord')) ?? null
    );
    const uuid = selected && typeof selected === 'object' ? selected.uuid : null;
    if (!uuid) {
      await statusBar.message('no metarecord is selected', statusMessageMs);
      return;
    }
    await editor.show(`same(mfr_duplicate_group, ${uuid})`);
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
    columns = parsed.length > 0 ? parsed : parseColumnsOr(defaultColumns, DEFAULT_COLUMNS);
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
    columns = parsed.length > 0 ? parsed : parseColumnsOr(defaultColumns, DEFAULT_COLUMNS);
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

  /** The columns drawn: the chosen ones, and the ordering field after them
   *  when the list is ordered by a field that is not one of them. */
  function shownColumns() {
    if (orderField === null || columns.some((c) => c.spec === orderField)) return columns;
    return [...columns, ...parseColumns(orderField)];
  }

  /** @param {Column} column */
  function toggleSort(column) {
    if (!isSortable(column)) return; // metarecord meta, not a sortable field
    if (orderField !== null) {
      // Ordered: the order is the field's, and a header must not hide it.
      void statusBar.message(
        `ordered by ${orderField} — metarecord-list:order off to sort by a column`,
        statusMessageMs,
      );
      return;
    }
    const current = sort.find((s) => s.field === column.name);
    sort = current
      ? current.order === 'asc'
        ? [{ field: column.name, order: 'desc' }]
        : []
      : [{ field: column.name, order: 'asc' }];
    queryRan = true;
    void fetchPage(true);
  }

  // ── Ordering by an int field (doc "Ordering a list", ./order-mode.js) ──

  const orderMode = createOrderMode({
    daemon,
    getRepo: () => repo,
    effectiveQuery,
    effectiveQueryText,
    getSort: () => sort,
  });
  const orderOnInput = byId(root, 'order-on', HTMLInputElement);
  const orderFieldInput = byId(root, 'order-field', HTMLInputElement);
  const orderFieldList = byId(root, 'order-fields');

  /** The box and the field beside it say what the list is ordered by. */
  function syncOrderControls() {
    orderOnInput.checked = orderField !== null;
    if (orderField !== null) orderFieldInput.value = orderField;
  }

  /** The repository's int fields, the ones that can order a list. */
  async function intFields() {
    if (!repo) return [];
    try {
      return (await daemon.fields(repo)).filter((f) => f.type === 'int').map((f) => f.name);
    } catch {
      return [];
    }
  }

  /** `order on <field>`: numbers what has no position, then sorts by it.
   *  @param {string} field */
  async function orderOn(field) {
    const name = (field ?? '').trim();
    const written = await orderMode.activate(name);
    if (written === null) return; // declined: nothing written, nothing changed
    orderField = name;
    sort = [{ field: name, order: 'asc' }];
    syncOrderControls();
    await workspace.set('metarecord-list:order-field', name);
    void statusBar.message(
      `ordered by ${name}` +
        (written > 0 ? ` — ${written} position${written === 1 ? '' : 's'} written` : ''),
      statusMessageMs,
    );
    queryRan = true;
    await fetchPage(true);
  }

  /** `order off`: the ordinary sort again; the positions stay. */
  async function orderOff() {
    if (orderField === null) return;
    orderField = null;
    sort = [];
    syncOrderControls();
    await workspace.set('metarecord-list:order-field', null);
    queryRan = true;
    await fetchPage(true);
  }

  /** Moves are applied one after the other: each reads the rows the previous
   *  one left. */
  let moveChain = Promise.resolve();

  /** `order move <where>` on the row under the cursor, which stays on it.
   *  @param {string} where */
  async function orderMove(where) {
    const field = orderField;
    if (field === null) {
      throw new Error('the list is not ordered: metarecord-list:order on <field> first');
    }
    if (!MOVES.includes(where)) {
      throw new Error(`unknown move: "${where ?? ''}" (expected ${MOVES.join(' / ')})`);
    }
    const moved = metarecords[cursorIndex];
    if (!moved) return;
    const rowsNow = metarecords.map((m) => ({ uuid: m.uuid, value: positionOf(m, field) }));
    const out = await orderMode.move(
      field,
      rowsNow,
      cursorIndex,
      /** @type {'up'|'down'|'top'|'bottom'} */ (where),
      nextCursor !== null,
    );
    if (out === null) {
      void statusBar.message(`nothing to move ${where}`, statusMessageMs);
      return;
    }
    // The loaded rows, in their new order, with their new positions: no read
    // again, so a move deep in a long list keeps the pages loaded.
    const before = new Map(rowsNow.map((r) => [r.uuid, r.value]));
    const byUuid = new Map(metarecords.map((m) => [m.uuid, m]));
    metarecords = out.map(({ uuid, value }) => {
      const m = /** @type {Metafolder.Metarecord} */ (byUuid.get(uuid));
      if (before.get(uuid) === value || value === null) return m;
      return {
        ...m,
        fields: [
          ...(m.fields ?? []).filter((f) => f.name !== field),
          { name: field, value: { type: 'int', value } },
        ],
      };
    });
    const index = metarecords.findIndex((m) => m.uuid === moved.uuid);
    render();
    await setCursor(index >= 0 ? index : cursorIndex);
  }

  /** @type {Record<string, (arg: string) => Promise<unknown>>} */
  const ORDER_ACTIONS = {
    on: (field) => orderOn(field),
    off: () => orderOff(),
    move: (where) => {
      const run = moveChain.then(() => orderMove(where));
      moveChain = run.catch(() => {});
      return run;
    },
  };

  orderOnInput.addEventListener('change', () => {
    const run = orderOnInput.checked ? orderOn(orderFieldInput.value) : orderOff();
    run.catch((error) => void statusBar.error(error)).finally(syncOrderControls);
  });
  // The completion of the field box: the int fields, read when it is entered.
  orderFieldInput.addEventListener('focus', () => {
    void intFields().then((names) =>
      orderFieldList.replaceChildren(...names.map((name) => el('option', { value: name }))),
    );
  });

  // ── Bulk edit (set/add/remove a field over the whole query result) ────

  const bulk = createBulkEdit({
    root,
    metafolder,
    getRepo: () => repo,
    effectiveQuery,
    statusMessageMs,
  });

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
    void commands.invoke('metarecord-list:apply simplified');
  });
  byId(root, 'columns-apply').addEventListener('click', () => {
    void commands.invoke('metarecord-list:apply columns');
  });
  // ── Finder (quick OSM filter) ─────────────────────────────────────────────

  function updateFinderFieldsLabel() {
    const names = finderFields.map((e) => e.split(':')[0]);
    finderFieldsLabel.textContent = names.join(' ');
    finderFieldsLabel.title = `finder searches: ${finderFields.join(', ')} (osm path / osmd direct)`;
  }

  /** Re-runs the query for the current finder text (debounced on input).
   *  A fetch still in flight for an earlier term is aborted and the query
   *  re-run once it unwinds (`fetchPage`), so the list ends on the current
   *  input without polling for it. */
  /** @param {{record?: boolean}} [options] */
  async function applyFinder({ record = false } = {}) {
    clearTimeout(finderTimer);
    queryRan = true;
    if (record) finderHistory.push(finderInput.value.trim());
    finderText = finderInput.value;
    await workspace.set('metarecord-list:finder', finderText);
    await fetchPage(true);
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
  columnsInput.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    void applyColumns();
    if (!event.shiftKey) columnsInput.blur();
  });

  // ── Wiring ──────────────────────────────────────────────────────────────

  // The cursor, for the generic commands (doc "Generic panel commands").
  // `more` loads the next page when `cursor:next` reaches the last row loaded:
  // the scroll pager only fires on a real scroll event of a displayed panel,
  // so keyboard/script navigation — which works just as well on a panel
  // sitting in a hidden workspace — has to ask for it itself.
  metafolder.provide({
    index: () => cursorIndex,
    count: () => metarecords.length,
    select: setCursor,
    more: () => (nextCursor ? fetchPage(false) : undefined),
  });
  void commands.register('metarecord-list:page', {
    label: 'Metarecord list: load a page (next)',
    args: [{ name: 'which', prompt: () => 'Which page? (next)', complete: () => ['next'] }],
    handler: (which) => {
      // Only forward: the list grows as it scrolls, it does not paginate back.
      if (which !== 'next') throw new Error(`unknown page: "${which ?? ''}" (expected next)`);
      return nextCursor ? fetchPage(false) : undefined;
    },
  });

  /** @type {Record<string, () => unknown>} */
  const SELECTIONS = {
    toggle: () => toggleChecked(),
    all: () => checkAll(),
    none: () => clearChecked(),
  };

  void commands.register('metarecord-list:select', {
    label: 'Metarecord list: change the multi-selection (toggle / all / none)',
    args: [
      {
        name: 'what',
        prompt: () => `Select what? (${Object.keys(SELECTIONS).join(' / ')})`,
        complete: () => Object.keys(SELECTIONS),
      },
    ],
    handler: (what) => {
      const run = SELECTIONS[what];
      if (!run) throw new Error(`unknown selection: "${what ?? ''}"`);
      return run();
    },
  });
  // ── Zones (commands in ./zone-commands.js) ──────────────────────────────
  //
  // The table below is the single place a zone's quirks live.
  //
  // The quirk that matters most: while zone B is *shown and frozen* it is what
  // the query runs (./query-editor.js), so anything that hands the user zone A
  // must unfreeze B first or the field it just focused feeds nothing.
  // Unfreezing discards a hand-edited B, which re-mirrors expand(A) — that is
  // the accepted trade in doc "metarecord-list panel".

  /** @type {Record<string, import('./zone-commands.js').Zone>} */
  const ZONES = {
    finder: {
      input: finderInput,
      // The explicit re-run also records the text in the finder's input
      // history; the debounced keystroke path does not.
      apply: () => applyFinder({ record: true }),
      clear: async () => {
        finderInput.value = '';
        // A live filter: clearing it re-runs at once, widening the result.
        await applyFinder();
      },
    },
    simplified: {
      input: editor.simplifiedInput,
      enter: () => editor.editSimplified(),
      apply: () => editor.apply(),
      clear: () => editor.clearSimplified(),
    },
    normal: {
      input: editor.normalInput,
      enter: () => editor.editNormal(),
      apply: () => editor.apply(),
      clear: () => editor.clearNormal(),
    },
    columns: {
      input: columnsInput,
      apply: () => applyColumns(),
      clear: () => {
        columnsInput.value = '';
      },
    },
  };

  registerZoneCommands({ commands, root, zones: ZONES, clearAll: clearAllQueries });

  // Canned queries: each puts its DSL in the zone, shown and frozen, so it
  // stays visible, editable and composable rather than being a mode.
  /** @type {Record<string, () => unknown>} */
  const CANNED_QUERIES = {
    duplicates: () => showDuplicates(),
    orphans: () => showOrphans(),
  };

  void commands.register('metarecord-list:query', {
    label: 'Metarecord list: run a canned query (duplicates / orphans)',
    args: [
      {
        name: 'what',
        prompt: () => `Which query? (${Object.keys(CANNED_QUERIES).join(' / ')})`,
        complete: () => Object.keys(CANNED_QUERIES),
      },
    ],
    handler: (what) => {
      const run = CANNED_QUERIES[what];
      if (!run) throw new Error(`unknown query: "${what ?? ''}"`);
      return run();
    },
  });

  // One command, the action as its first argument (doc "Command naming"): the
  // field always comes second, so no field name can be mistaken for `off`.
  void commands.register('metarecord-list:order', {
    label: 'Metarecord list: order by an int field (on <field> / off / move <up/down/top/bottom>)',
    args: [
      {
        name: 'action',
        prompt: () => `Order: ${Object.keys(ORDER_ACTIONS).join(' / ')}?`,
        complete: () => Object.keys(ORDER_ACTIONS),
      },
      {
        name: 'arg',
        when: (p) => p[0] !== 'off',
        prompt: (p) =>
          p[0] === 'move' ? `Move where? (${MOVES.join(' / ')})` : 'Order by which int field?',
        complete: async (_partial, p) => (p[0] === 'move' ? MOVES : await intFields()),
      },
    ],
    handler: (action, arg) => {
      const run = ORDER_ACTIONS[action];
      if (!run) {
        throw new Error(
          `unknown order action: "${action ?? ''}" (expected ${Object.keys(ORDER_ACTIONS).join(' / ')})`,
        );
      }
      return run(arg);
    },
  });

  // `mode` flips the table ⇄ the grid through the `mode` setting, so the
  // toggle and `metarecord-list:set mode` persist the same variable.
  /** @type {Record<string, () => unknown>} */
  const TOGGLES = {
    normal: () => editor.toggleNormal(),
    mode: () => SETTINGS.mode.apply(mode === 'grid' ? 'table' : 'grid'),
  };

  void commands.register('metarecord-list:toggle', {
    label: `Metarecord list: toggle a view flag (${Object.keys(TOGGLES).join(' / ')})`,
    args: [
      {
        name: 'flag',
        prompt: () => `Which flag? (${Object.keys(TOGGLES).join(' / ')})`,
        complete: () => Object.keys(TOGGLES),
      },
    ],
    handler: (flag) => {
      const toggle = TOGGLES[flag];
      if (!toggle)
        throw new Error(
          `unknown flag: "${flag ?? ''}" (expected ${Object.keys(TOGGLES).join(' / ')})`,
        );
      return toggle();
    },
  });

  // Read again on `view:refresh` (doc "Generic panel commands").
  // A refresh is a start: it reads the active repository too, so it loads even
  // when it comes before the first display has started the panel (a script
  // showing the list then refreshing it), and it always loads — `start` arms
  // the query whenever there is a repository.
  metafolder.provide({ refresh: () => start() });
  // Two entry points to bulk editing (doc "Editing metarecords in the GUI"):
  // this one is the mouse-oriented half — the in-panel form (op picker + value
  // widget), opened by the footer button. The keyboard half is
  // `metarecord:bulk <op>`, which lives with the commands that perform it.
  void commands.register('metarecord-list:open-bulk-edit', {
    label: 'Metarecord list: open the bulk edit / delete form (set/add/remove/unset/delete)',
    reveal: true,
    handler: () => bulk.toggle(),
  });
  /** Lays the rows out as `value` says; anything that is no mode (a stale or
   *  mistyped stored value) is the table. @param {unknown} value */
  function applyMode(value) {
    mode = value === 'grid' ? 'grid' : 'table';
    bodyEl.classList.toggle('grid', mode === 'grid');
  }

  // View settings: what is displayed, not what is stored. `set` names the
  // setting as its first argument, so a new one costs a table entry.
  /** @type {Record<string, {values?: string[], apply: (value: string) => unknown}>} */
  const SETTINGS = {
    mode: {
      values: ['table', 'grid'],
      apply: (value) => {
        if (value !== 'table' && value !== 'grid')
          throw new Error(`unknown mode: "${value ?? ''}" (expected table / grid)`);
        applyMode(value);
        return workspace.set('metarecord-list:mode', value);
      },
    },
    'page-size': {
      apply: async (raw) => {
        const n = Math.floor(Number(raw));
        if (!Number.isFinite(n) || n < 1) throw new Error(`invalid page size: "${raw ?? ''}"`);
        await workspace.set('metarecord-list:page-size', n);
      },
    },
  };

  void commands.register('metarecord-list:set', {
    label: `Metarecord list: change a view setting (${Object.keys(SETTINGS).join(' / ')})`,
    args: [
      {
        name: 'setting',
        prompt: () => `Which setting? (${Object.keys(SETTINGS).join(' / ')})`,
        complete: () => Object.keys(SETTINGS),
      },
      {
        name: 'value',
        prompt: (p) => `Value for "${p[0]}"?`,
        complete: (_partial, p) => SETTINGS[p[0]]?.values ?? [],
      },
    ],
    handler: (setting, value) => {
      const found = SETTINGS[setting];
      if (!found) throw new Error(`unknown setting: "${setting ?? ''}"`);
      return found.apply(value);
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
  // the keyboard cursor last sat on — `metarecord:remove`/`metarecord:trash`/
  // `pick:confirm` read `selected_metarecord`, which the cursor keeps in sync.
  // All actions confirm where they mutate anything.
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
      // Checking from the menu, as Space does from the keyboard: the row joins
      // (or leaves) the checked selection the bulk operations act on.
      trailing: [
        {
          label: selection.has(target.uuid) ? 'Remove from the selection' : 'Add to the selection',
          action: () => void selection.toggle(target.uuid),
        },
      ],
    });
    if (repo && paths.length > 0) {
      // The "File" category (cut / copy / paste / rename / duplicate / move to
      // trash), shared with the file manager — see /__file-actions.js. No
      // separator to push: /__menu.js draws one at the category boundary.
      items.push(
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
      queryRan = false;
      fetchError = null;
      metarecords = [];
      nextCursor = null;
      total = null;
      countSeq += 1;
      cursorIndex = -1;
      display.reset();
      void display.refreshMounts();
      // The checked selection is not cleared here: /__multi-select.js owns the
      // policy (a genuine repository *switch* clears it, adopting one on a
      // fresh panel does not — a remount keeps what is already checked).
    }
    byId(root, 'no-repo').hidden = repo !== null;
    if (!queryInitialized) {
      queryInitialized = true;
      await editor.recompute();
    }
    if (repo !== null) {
      // Read the field catalog so the finder can auto-detect osm/osmd per field.
      await readFieldTypes();
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
  // quiet period (GET /watch) — after our immediate refresh has already read the
  // stale path. Poll the change feed a couple of times over the next seconds so
  // the settled change is picked up promptly (the subscription above then
  // repaints), rather than waiting for the slow background poll. The 7 s timer
  // remains the backstop.
  const catchup = createCatchup(daemon);
  function scheduleCatchupSync() {
    if (!repo) return;
    const target = repo;
    catchup.schedule(target, [200, 1300], () => void changes.sync(target));
  }
  // A selection made by another panel (the file panel's slideshow, a click in
  // the recent panel) moves the cursor onto its row when the list has it
  // loaded, so the arrows go on from the file on screen. Only the highlight
  // moves: the selection is already what it should be, and propagating it
  // again would only echo it back.
  workspace.onChange('selected_metarecord', (value) => {
    const uuid = /** @type {{uuid?: unknown}|null} */ (value)?.uuid;
    if (typeof uuid !== 'string') return;
    const echoes = pendingEchoes.get(uuid) ?? 0;
    if (echoes > 0) {
      if (echoes === 1) pendingEchoes.delete(uuid);
      else pendingEchoes.set(uuid, echoes - 1);
      return;
    }
    if (metarecords[cursorIndex]?.uuid === uuid) return;
    const index = metarecords.findIndex((m) => m.uuid === uuid);
    if (index < 0) return;
    cursorIndex = index;
    cursor.highlight(index);
    cursor.reveal();
  });
  workspace.onChange('metarecords:dirty', () => {
    metafolder.whenVisible(deferredStart);
    scheduleCatchupSync();
  });
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));

  // Keep the list live when the daemon reflects a change out-of-band from our
  // own query round-trip — chiefly a watcher-driven update (a GUI rename lands
  // in the daemon a quiet period after the fs.move, well after our immediate
  // `metarecords:dirty` refresh already read the stale path). Every change of
  // the repository re-reads the displayed rows' paths, not only a change *to*
  // a displayed row: a path spells every ancestor's name, and renaming a
  // directory writes that directory alone — the rows below it, which are what
  // is on screen, are never named by the feed.
  const unsubscribeChanges = changes.subscribe(({ repo: changedRepo, uuids }) => {
    if (changedRepo !== repo || metarecords.length === 0) return;
    display.forgetOrphans(uuids); // null: the whole repo may have changed
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
  workspace.onChange('metarecord-list:mode', applyMode);
  workspace.onChange('metarecord-list:finder-fields', (value) => {
    finderFields = Array.isArray(value) && value.length ? value : defaultFinderFields.slice();
    updateFinderFieldsLabel();
    if (queryRan) void fetchPage(true);
  });

  setColumns(await workspace.get('metarecord-list:columns'));
  // Adopt the checked selection (shared with the file manager, kept across
  // list changes) before the first fetch can repaint the rows.
  await selection.load();
  widths = /** @type {Record<string, number>} */ (
    (await workspace.get('metarecord-list:column-widths')) ?? {}
  );
  pageSize = sanitizePageSize(await workspace.get('metarecord-list:page-size'));
  applyMode(await workspace.get('metarecord-list:mode'));

  // Restore the finder (quick filter) state.
  finderText = asText(await workspace.get('metarecord-list:finder'));
  finderInput.value = finderText;
  const storedFinderFields = await workspace.get('metarecord-list:finder-fields');
  finderFields =
    Array.isArray(storedFinderFields) && storedFinderFields.length
      ? storedFinderFields
      : defaultFinderFields.slice();
  updateFinderFieldsLabel();

  await editor.restore();

  // An ordered list comes back ordered — sorted, nothing written.
  const storedOrder = await workspace.get('metarecord-list:order-field');
  if (typeof storedOrder === 'string' && storedOrder) {
    orderField = storedOrder;
    sort = [{ field: storedOrder, order: 'asc' }];
  }
  syncOrderControls();

  metafolder.whenVisible(deferredStart);

  return () => {
    clearTimeout(finderTimer);
    editor.dispose();
    catchup.cancel();
    finderHistory.detach();
    queryHistory.detach();
    unsubscribeChanges();
    document.removeEventListener('mousemove', /** @type {EventListener} */ (onMouseMove));
    document.removeEventListener('mouseup', onMouseUp);
    detachScroll();
  };
}
