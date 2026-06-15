// metarecord-list panel: metarecords of the active repo filtered by an embedded
// DSL query; primary selection source (spec-gui "metarecord-list panel type").

import { el, fields } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { parseColumns, isSortable, cellQuickText, cellText, fillColumns, treeRefFields, refTargetUuids } from './columns.js';

const DEFAULT_PAGE_SIZE = 100;
const DEFAULT_COLUMNS = 'mfr_path~ mfr_type &version';
const GRID_NAME_COLUMN = parseColumns('mfr_path~')[0];

// "Empty query matches all": three-valued tautology on any field.
const MATCH_ALL = {
  type: 'or',
  operands: [
    { type: 'is_present', field: 'mfr_path' },
    { type: 'is_absent', field: 'mfr_path' },
    { type: 'is_unknown', field: 'mfr_path' },
  ],
};

export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, query, bench, cache } = metafolder;
  const REFRESH = cache.REFRESH;

  let repo = null;
  let columns = parseColumns(DEFAULT_COLUMNS); // persisted per workspace (spec strings)
  let widths = {}; // column spec -> px; persisted per workspace
  let metarecords = [];
  let nextCursor = null;
  let total = null; // full result count (daemon-side COUNT, first page only)
  let pageSize = DEFAULT_PAGE_SIZE; // persisted per workspace
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
        cache.fetchTreeRefs(repo, field, subset.filter((m) => hasTreeRef(m, field)).map((m) => m.uuid)),
      ),
    );
    await cache.fetchMetarecords(repo, refTargetUuids(columns, subset));
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
    const targets = new Map();
    for (const uuid of refTargetUuids(columns, subset)) {
      const target = cache.readMetarecord(repo, uuid);
      targets.set(uuid, target === REFRESH ? null : target);
    }
    fillColumns(columns, subset, { pathsByField, targets });
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
      let keepUuid = null;
      if (reset) {
        // A refresh must not steal the selection: remember the highlighted
        // metarecord and restore it.
        keepUuid =
          metarecords[cursorIndex]?.uuid ?? (await workspace.get('selected_metarecord'))?.uuid ?? null;
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
      const fetched = result.uuids.map((u) => cache.readMetarecord(repo, u)).filter((m) => m !== REFRESH);
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
        const keepIndex = keepUuid === null ? -1 : metarecords.findIndex((e) => e.uuid === keepUuid);
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
    widths[resizing.column.spec] = Math.max(40, resizing.startWidth + event.clientX - resizing.startX);
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
      orphanCache.set(metarecord.uuid, orphanState(metarecord, orphanCtx).catch(() => null));
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
            class: ['row', index === cursorIndex && 'cursor', checked.has(metarecord.uuid) && 'checked'],
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
        const img = el('img', { loading: 'lazy' });
        fillThumbnail(img, metarecord);
        const card = el(
          'div',
          {
            class: ['card', index === cursorIndex && 'cursor'],
            onclick: () => setCursor(index),
            ondblclick: () => openSelected(),
          },
          img,
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

  function fillThumbnail(img, metarecord) {
    const path = pathsOf(metarecord)[0];
    if (path) img.src = `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(path)}`;
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
    await workspace.set('metarecord-list:columns', columns.map((c) => c.spec));
  }

  /** A stored/typed page size; anything invalid falls back to the default. */
  function sanitizePageSize(value) {
    const n = Math.floor(Number(value));
    return Number.isFinite(n) && n >= 1 ? n : DEFAULT_PAGE_SIZE;
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

  scroll.addEventListener('scroll', () => {
    if (nextCursor && scroll.scrollTop + scroll.clientHeight > scroll.scrollHeight - 200) {
      void fetchPage(false);
    }
  });

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
  };
}
