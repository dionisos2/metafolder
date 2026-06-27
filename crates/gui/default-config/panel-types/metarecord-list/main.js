// metarecord-list panel: metarecords of the active repo filtered by an embedded
// DSL query; primary selection source (spec-gui "metarecord-list panel type").

import { el, fields, thumbnail } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { createPagedList } from '/__paged-list.js';
import { createTypePicker, widgetFor, bulkSetBody, MATCH_ALL } from '/__value-widget.js';
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
const GRID_NAME_COLUMN = parseColumns('mfr_path:path')[0];

export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, query, bench, cache } = metafolder;
  const REFRESH = cache.REFRESH;
  const defaultPageSize = metafolder.pageSize ?? DEFAULT_PAGE_SIZE_FALLBACK;

  let repo = null;
  let columns = parseColumns(DEFAULT_COLUMNS); // persisted per workspace (spec strings)
  let widths = {}; // column spec -> px; persisted per workspace
  let metarecords = [];
  let nextCursor = null;
  let total = null; // full result count (daemon-side COUNT, first page only)
  let pageSize = defaultPageSize; // persisted per workspace
  let loading = false;
  let queryIR = null; // null = match all
  let normalShown = false; // zone B (normal DSL) revealed?
  let normalFrozen = false; // zone B decoupled (hand-edited, authoritative)?
  let queryInitialized = false; // first query compiled on first display
  let livePreviewTimer = null;
  let sort = []; // [{field, order}]
  let cursorIndex = -1;
  let checked = new Set(); // multi-selection (uuids)
  let mode = 'table';
  let orphanCache = new Map(); // uuid -> Promise<null|'deleted'|'missing'>

  const bodyEl = root.querySelector('.mf-panel-body');
  const rows = root.getElementById('rows');
  const grid = root.getElementById('grid');
  const scroll = root.getElementById('scroll');
  const statusLine = root.getElementById('status-line');
  const queryInput = root.getElementById('query-input');
  const columnsInput = root.getElementById('columns-input');
  const queryError = root.getElementById('query-error');
  const columnsError = root.getElementById('columns-error');
  const normalToggle = root.getElementById('normal-toggle');
  const normalEditor = root.getElementById('normal-editor');
  const normalInput = root.getElementById('normal-input');
  const normalError = root.getElementById('normal-error');
  const normalFreeze = root.getElementById('normal-freeze');
  const bulkForm = root.getElementById('bulk-form');
  const bulkOp = root.getElementById('bulk-op');
  const bulkName = root.getElementById('bulk-name');
  const bulkValueSlot = root.getElementById('bulk-value');
  const bulkForce = root.getElementById('bulk-force');
  const bulkError = root.getElementById('bulk-error');

  // ── Data access (all daemon data comes from the shared cache) ─────────────

  let repoRoot = null; // absolute root path of the active repo (cached once)

  function hasTreeRef(metarecord, field) {
    return fields(metarecord, field).some((f) => f.value.type === 'tree_ref');
  }

  const treeFieldsOf = () => new Set(['mfr_path', ...treeRefFields(columns)]);

  // Pre-fetches the display data for the ~ columns into the shared cache, then
  // fills the columns from cache reads — rendering stays synchronous and never
  // mutates the (shared, read-only) cached metarecords.
  function prepare(subset) {
    return bench.measure('mf:list:enrich', () => prepareNow(subset));
  }

  async function prepareNow(subset) {
    if (subset.length === 0) return;
    if (repoRoot === null) repoRoot = await daemon.repoRoot(repo);
    await Promise.all(
      [...treeFieldsOf()].map((field) =>
        cache.fetchTreeRefs(
          repo,
          field,
          subset.filter((m) => hasTreeRef(m, field)).map((m) => m.uuid),
        ),
      ),
    );
    const targetUuids = refTargetUuids(columns, subset);
    await cache.fetchMetarecords(repo, targetUuids);
    // Phase 2: `tag>path:path` columns also need the followed targets' own tree
    // paths resolved (same machinery, on the target uuids).
    await Promise.all(
      followedTreeFields(columns).map((field) =>
        cache.fetchTreeRefs(
          repo,
          field,
          targetUuids.filter((u) => {
            const t = cache.readMetarecord(repo, u);
            return t !== REFRESH && hasTreeRef(t, field);
          }),
        ),
      ),
    );
    fillFromCache(subset);
  }

  function fillFromCache(subset) {
    const pathsByField = {};
    for (const field of treeFieldsOf()) {
      pathsByField[field] = {};
      for (const m of subset) {
        const paths = cache.readTreeRef(repo, field, m.uuid);
        if (paths !== REFRESH) pathsByField[field][m.uuid] = paths;
      }
    }
    const targetUuids = refTargetUuids(columns, subset);
    const targets = new Map();
    for (const uuid of targetUuids) {
      const target = cache.readMetarecord(repo, uuid);
      targets.set(uuid, target === REFRESH ? null : target);
    }
    const followedPathsByField = {};
    for (const field of followedTreeFields(columns)) {
      followedPathsByField[field] = {};
      for (const uuid of targetUuids) {
        const paths = cache.readTreeRef(repo, field, uuid);
        if (paths !== REFRESH) followedPathsByField[field][uuid] = paths;
      }
    }
    fillColumns(columns, subset, { pathsByField, targets, followedPathsByField });
  }

  // Absolute filesystem paths of a metarecord's mfr_path positions (read-only,
  // from the cache + the repo root) — replaces the old per-metarecord `.paths`.
  function pathsOf(metarecord) {
    const rel = cache.readTreeRef(repo, 'mfr_path', metarecord.uuid);
    if (rel === REFRESH || repoRoot === null) return [];
    return rel.map((r) => (r === '' ? repoRoot : `${repoRoot}/${r}`));
  }

  /** Re-derives the ~ columns over the loaded metarecords (after a column change). */
  async function reresolveColumns() {
    await prepareNow(metarecords);
  }

  async function fetchPage(reset) {
    if (!repo || loading) return;
    loading = true;
    try {
      // A reset fetch is a deliberate freshness point (query, refresh, display):
      // poll the change feed so stale cached data is dropped before we read.
      if (reset) await cache.sync(repo);
      let keepUuid = null;
      if (reset) {
        // A refresh must not steal the selection: remember the highlighted
        // metarecord and restore it.
        keepUuid =
          metarecords[cursorIndex]?.uuid ??
          (await workspace.get('selected_metarecord'))?.uuid ??
          null;
        metarecords = [];
        nextCursor = null;
        orphanCache = new Map();
      }
      let result;
      try {
        result = await cache.query(repo, {
          query: queryIR ?? MATCH_ALL,
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
      const fetched = result.uuids
        .map((u) => cache.readMetarecord(repo, u))
        .filter((m) => m !== REFRESH);
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

  let resizing = null; // {column, startX, startWidth, moved}

  function startResize(event, column) {
    event.preventDefault();
    resizing = {
      column,
      startX: event.clientX,
      startWidth: event.target.closest('th').offsetWidth,
      moved: false,
    };
  }

  // Document-level so the drag keeps tracking outside the column; removed on
  // unmount (cleanup) so they do not leak across panel instances.
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
  document.addEventListener('mousemove', onMouseMove);
  document.addEventListener('mouseup', onMouseUp);

  function renderHeader() {
    root.getElementById('header-row').replaceChildren(
      ...columns.map((column) => {
        const active = isSortable(column) && sort.find((s) => s.field === column.name);
        const th = el(
          'th',
          { onclick: () => toggleSort(column) },
          column.spec + (active ? (active.order === 'asc' ? ' ▲' : ' ▼') : ''),
          el('div', {
            class: 'col-resize',
            onmousedown: (event) => startResize(event, column),
            onclick: (event) => event.stopPropagation(),
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
    metarecordPaths: (metarecord) => pathsOf(metarecord),
    exists: (path) =>
      metafolder.fs.stat(path).then(
        () => true,
        () => false,
      ),
  };

  /** Marks the row/card when the metarecord's tracked file is gone (async). */
  function fillOrphan(node, metarecord) {
    if (!orphanCache.has(metarecord.uuid)) {
      orphanCache.set(
        metarecord.uuid,
        orphanState(metarecord, orphanCtx).catch(() => null),
      );
    }
    void orphanCache.get(metarecord.uuid).then((state) => {
      if (state === null) return;
      node.classList.add('orphan');
      node.title = orphanLabel(state);
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
    statusLine.textContent =
      `${metarecords.length}${total !== null ? `/${total}` : ''} metarecord${
        (total ?? metarecords.length) === 1 ? '' : 's'
      }` +
      (nextCursor ? ' (more available — scroll down)' : '') +
      (checked.size > 0 ? ` — ${checked.size} selected` : '');
  }

  // ── Selection (workspace variables) ─────────────────────────────────────

  async function setCursor(index) {
    cursorIndex = Math.max(0, Math.min(index, metarecords.length - 1));
    render();
    const metarecord = metarecords[cursorIndex];
    if (!metarecord) return;
    root.querySelector('tr.cursor')?.scrollIntoView({ block: 'nearest' });
    await workspace.set('selected_metarecord', { uuid: metarecord.uuid, repo });
    await workspace.set('selected_paths', pathsOf(metarecord));
  }

  async function toggleChecked() {
    const metarecord = metarecords[cursorIndex];
    if (!metarecord) return;
    if (checked.has(metarecord.uuid)) checked.delete(metarecord.uuid);
    else checked.add(metarecord.uuid);
    render();
    await workspace.set('selected_metarecords', [...checked]);
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
    let dsl;
    if (normalShown && normalFrozen) {
      dsl = normalInput.value.trim(); // frozen normal DSL is authoritative
    } else {
      const simplified = queryInput.value.trim();
      if (simplified === '') {
        dsl = '';
      } else {
        try {
          dsl = (await query.expand(simplified)).trim();
        } catch (error) {
          queryError.textContent = String(error.message ?? error);
          return false;
        }
      }
      if (normalShown) normalInput.value = dsl; // reflect in B
    }
    if (dsl === '') {
      queryIR = null; // empty = match all
    } else {
      try {
        queryIR = await query.parse(dsl);
      } catch (error) {
        (normalShown ? normalError : queryError).textContent = String(error.message ?? error);
        return false;
      }
    }
    return true;
  }

  async function applyQuery() {
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
    livePreviewTimer = setTimeout(() => void refreshPreview(), 130);
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
      normalInput.value = (await query.expand(simplified)).trim();
    } catch (error) {
      queryError.textContent = String(error.message ?? error);
    }
  }

  async function setNormalShown(shown) {
    normalShown = shown;
    normalEditor.hidden = !shown;
    normalToggle.textContent = shown ? 'Hide normal DSL' : 'Show normal DSL';
    if (shown && !normalFrozen) await refreshPreview();
    await workspace.set('metarecord-list:normal-shown', shown);
  }

  async function setNormalFrozen(frozen) {
    normalFrozen = frozen;
    normalFreeze.checked = frozen;
    normalInput.readOnly = !frozen;
    if (!frozen && normalShown) await refreshPreview();
    await workspace.set('metarecord-list:normal-frozen', frozen);
  }

  // ── Columns ─────────────────────────────────────────────────────────────

  function setColumns(value) {
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
    let parsed;
    try {
      parsed = parseColumns(columnsInput.value);
    } catch (error) {
      columnsError.textContent = String(error.message ?? error);
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
  function sanitizePageSize(value) {
    const n = Math.floor(Number(value));
    return Number.isFinite(n) && n >= 1 ? n : defaultPageSize;
  }

  function toggleSort(column) {
    if (!isSortable(column)) return; // metarecord meta, not a sortable field
    const current = sort.find((s) => s.field === column.name);
    sort = current
      ? current.order === 'asc'
        ? [{ field: column.name, order: 'desc' }]
        : []
      : [{ field: column.name, order: 'asc' }];
    void fetchPage(true);
  }

  // ── Bulk edit (set/append/remove a field over the whole query result) ────

  let bulkWidget = null; // {element, read()} following the picked type

  // Each operation maps to its batch endpoint and a confirmation verb.
  // `valueless` ops (unset) act on the field name alone — no value widget.
  const BULK_OPS = {
    set: { path: 'query/fields/set', verb: 'Set', prep: 'on' },
    append: { path: 'query/fields/append', verb: 'Append', prep: 'to' },
    remove: { path: 'query/fields/remove', verb: 'Remove', prep: 'from' },
    unset: { path: 'query/fields/unset', verb: 'Unset', prep: 'from', valueless: true },
  };

  /** The form's value widget follows the picked type. */
  function setBulkWidget(type) {
    bulkWidget = widgetFor(type, undefined);
    bulkValueSlot.replaceChildren(bulkWidget.element);
  }
  const bulkTypePicker = createTypePicker(
    root.getElementById('bulk-type'),
    'string',
    setBulkWidget,
  );
  setBulkWidget(bulkTypePicker.get());

  // Hide the type picker + value row for value-less ops (unset).
  const bulkValueRow = root.getElementById('bulk-value-row');
  const bulkTypeBtn = root.getElementById('bulk-type');
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
    const result = await daemon.call('POST', `/repos/${repo}/query`, {
      query: queryIR ?? MATCH_ALL,
      select: '*',
      limit: 1,
      count: true,
    });
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
      // Value-less ops (unset) act on the name alone.
      const body = op.valueless
        ? { query: queryIR ?? MATCH_ALL, name, ...(force ? { force: true } : {}) }
        : bulkSetBody(queryIR, name, bulkWidget.read(), force);
      const resp = await daemon.call('POST', `/repos/${repo}/${op.path}`, body);
      const updated = resp.updated ?? 0;
      bulkForm.classList.remove('open');
      statusBar.message(
        `${op.verb} "${name}": ${updated} metarecord${updated === 1 ? '' : 's'} changed.`,
        5000,
      );
      // Refresh this list and any metarecord-detail mirror (the cache picks the
      // write up via the change feed on the next reset fetch).
      await workspace.set('metarecords:dirty', Date.now());
    } catch (error) {
      bulkError.textContent = String(error.message ?? error);
    }
  }

  // Progressive loading: the shared controller owns the scroll threshold and
  // the one-fetch-at-a-time guard; the footer below stays custom (it carries
  // the selection count too). hasMore tracks the daemon cursor.
  const pager = createPagedList({
    loaded: () => metarecords.length,
    total: () => total,
    hasMore: () => nextCursor !== null,
    loadMore: () => fetchPage(false),
  });
  const detachScroll = pager.attach(scroll);

  root
    .getElementById('query-apply')
    .addEventListener('click', () => void commands.invoke('metarecord-list:apply-query'));
  root
    .getElementById('columns-apply')
    .addEventListener('click', () => void commands.invoke('metarecord-list:apply-columns'));
  queryInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyQuery();
  });
  queryInput.addEventListener('input', scheduleLivePreview);
  normalToggle.addEventListener('click', () => void setNormalShown(!normalShown));
  normalFreeze.addEventListener('change', () => void setNormalFrozen(normalFreeze.checked));
  normalInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyQuery();
  });
  columnsInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') void applyColumns();
  });
  root
    .getElementById('bulk-open')
    .addEventListener('click', () => void commands.invoke('metarecord-list:bulk-edit'));
  root.getElementById('bulk-apply').addEventListener('click', () => void applyBulkEdit());
  root
    .getElementById('bulk-cancel')
    .addEventListener('click', () => bulkForm.classList.remove('open'));
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
    handler: toggleChecked,
  });
  commands.register('metarecord-list:open', {
    label: 'Metarecord list: open the selection in the other panel',
    handler: openSelected,
  });
  commands.register('metarecord-list:set-mode', {
    label: 'Metarecord list: switch display mode (table | grid)',
    handler: (newMode) => {
      mode = newMode === 'grid' ? 'grid' : 'table';
      bodyEl.classList.toggle('grid', mode === 'grid');
    },
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
    handler: () => fetchPage(true),
  });
  commands.register('metarecord-list:bulk-edit', {
    label: 'Metarecord list: set/append/remove a field on every metarecord matching the query',
    reveal: true,
    handler: openBulkForm,
  });
  commands.register('metarecord-list:set-page-size', {
    label: 'Metarecord list: set the page size (results per fetch)',
    handler: async (raw) => {
      const n = Math.floor(Number(raw));
      if (!Number.isFinite(n) || n < 1) throw new Error(`invalid page size: "${raw ?? ''}"`);
      await workspace.set('metarecord-list:page-size', n);
    },
  });

  metafolder.addKeybinding('metarecord-list:next', 'down');
  metafolder.addKeybinding('metarecord-list:next', 'j');
  metafolder.addKeybinding('metarecord-list:prev', 'up');
  metafolder.addKeybinding('metarecord-list:prev', 'k');
  metafolder.addKeybinding('metarecord-list:first', 'home');
  metafolder.addKeybinding('metarecord-list:last', 'end');
  metafolder.addKeybinding('metarecord-list:select-toggle', 'space');
  metafolder.addKeybinding('metarecord-list:open', 'enter');
  metafolder.addKeybinding('metarecord-list:open', 'right');
  metafolder.addKeybinding('metarecord-list:edit-query', '/');

  async function start() {
    repo = await workspace.get('active_repo');
    root.getElementById('no-repo').hidden = repo !== null;
    if (!queryInitialized) {
      queryInitialized = true;
      await recomputeQuery();
    }
    if (repo !== null) await fetchPage(true);
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
    void fetchPage(true);
  });

  setColumns(await workspace.get('metarecord-list:columns'));
  widths = (await workspace.get('metarecord-list:column-widths')) ?? {};
  pageSize = sanitizePageSize(await workspace.get('metarecord-list:page-size'));

  // Restore the two-zone query editor (values only — no daemon call here).
  queryInput.value = (await workspace.get('metarecord-list:query')) ?? '';
  normalInput.value = (await workspace.get('metarecord-list:normal-query')) ?? '';
  normalFrozen = (await workspace.get('metarecord-list:normal-frozen')) ?? false;
  normalFreeze.checked = normalFrozen;
  normalInput.readOnly = !normalFrozen;
  normalShown = (await workspace.get('metarecord-list:normal-shown')) ?? false;
  normalEditor.hidden = !normalShown;
  normalToggle.textContent = normalShown ? 'Hide normal DSL' : 'Show normal DSL';

  metafolder.whenVisible(deferredStart);

  return () => {
    document.removeEventListener('mousemove', onMouseMove);
    document.removeEventListener('mouseup', onMouseUp);
    detachScroll();
  };
}
