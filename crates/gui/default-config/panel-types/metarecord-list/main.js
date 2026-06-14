// metarecord-list panel: metarecords of the active repo filtered by an embedded
// DSL query; primary selection source (spec-gui "metarecord-list panel type").

import { el } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { parseColumns, isSortable, cellQuickText, cellText } from './columns.js';

const { daemon, workspace, commands, statusBar, query } = metafolder;

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
let refCache = new Map(); // uuid -> Promise<metarecord>, for ~target columns
let orphanCache = new Map(); // uuid -> Promise<null|'deleted'|'missing'>

const rows = document.getElementById('rows');
const grid = document.getElementById('grid');
const scroll = document.getElementById('scroll');
const statusLine = document.getElementById('status-line');
const queryInput = document.getElementById('query-input');
const columnsInput = document.getElementById('columns-input');
const queryError = document.getElementById('query-error');
const columnsError = document.getElementById('columns-error');
const normalToggle = document.getElementById('normal-toggle');
const normalEditor = document.getElementById('normal-editor');
const normalInput = document.getElementById('normal-input');
const normalError = document.getElementById('normal-error');
const normalFreeze = document.getElementById('normal-freeze');

// ── Data access ─────────────────────────────────────────────────────────

// Context for ~ column display (columns.js); the ref target cache lives
// for one result set and is dropped on every reset fetch.
const displayCtx = {
  resolveTreeRef: (value) => daemon.resolveTreeRef(repo, value),
  getMetarecord: (uuid) => {
    if (!refCache.has(uuid)) {
      refCache.set(uuid, daemon.call('GET', `/repos/${repo}/metarecords/${uuid}`));
    }
    return refCache.get(uuid);
  },
};

async function fetchPage(reset) {
  if (!repo || loading) return;
  loading = true;
  try {
    let keepUuid = null;
    if (reset) {
      // A refresh (new query, metarecords:dirty, …) must not steal the
      // selection: remember the highlighted metarecord and restore it.
      keepUuid =
        metarecords[cursorIndex]?.uuid ?? (await workspace.get('selected_metarecord'))?.uuid ?? null;
      metarecords = [];
      nextCursor = null;
      refCache = new Map();
      orphanCache = new Map();
    }
    let page;
    try {
      page = await daemon.call('POST', `/repos/${repo}/query`, {
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
    metarecords = metarecords.concat(page.results);
    nextCursor = page.next_cursor;
    if (reset) total = page.total ?? null;
    if (reset) {
      // Drop checked metarecords that no longer match.
      const alive = new Set(metarecords.map((e) => e.uuid));
      if ([...checked].some((uuid) => !alive.has(uuid))) {
        checked = new Set([...checked].filter((uuid) => alive.has(uuid)));
        await workspace.set('selected_metarecords', [...checked]);
      }
      const keepIndex = keepUuid === null ? -1 : metarecords.findIndex((e) => e.uuid === keepUuid);
      if (keepIndex >= 0) {
        // Same metarecord still listed: move the cursor back silently, the
        // selection variables are already correct.
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

// ── Rendering ───────────────────────────────────────────────────────────

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

document.addEventListener('mousemove', (event) => {
  if (!resizing) return;
  resizing.moved = true;
  widths[resizing.column.spec] = Math.max(
    40,
    resizing.startWidth + event.clientX - resizing.startX,
  );
  renderHeader();
});

document.addEventListener('mouseup', () => {
  if (!resizing) return;
  const { moved } = resizing;
  resizing = null;
  if (moved) void workspace.set('metarecord-list:column-widths', { ...widths });
});

function renderHeader() {
  document.getElementById('header-row').replaceChildren(
    ...columns.map((column) => {
      const active = isSortable(column) && sort.find((s) => s.field === column.name);
      const th = el(
        'th',
        { onclick: () => toggleSort(column) },
        column.spec + (active ? (active.order === 'asc' ? ' ▲' : ' ▼') : ''),
        el('div', {
          class: 'col-resize',
          onmousedown: (event) => startResize(event, column),
          // A click right after a resize drag must not toggle the sort.
          onclick: (event) => event.stopPropagation(),
        }),
      );
      if (widths[column.spec]) th.style.width = `${widths[column.spec]}px`;
      return th;
    }),
  );
}

function fillCell(node, column, metarecord) {
  cellText(column, metarecord, displayCtx).then(
    (text) => (node.textContent = text),
    () => {}, // keep the quick text
  );
}

// Orphan check environment (one disk stat per metarecord per result set).
const orphanCtx = {
  metarecordPaths: (metarecord) => daemon.metarecordPaths(repo, metarecord),
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
        columns.map((column) => {
          const td = el('td', {}, cellQuickText(column, metarecord));
          if (column.deref !== null) fillCell(td, column, metarecord);
          return td;
        }),
      );
      fillOrphan(tr, metarecord);
      return tr;
    }),
  );
  grid.replaceChildren(
    ...metarecords.map((metarecord, index) => {
      const img = el('img', { loading: 'lazy' });
      void fillThumbnail(img, metarecord);
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

async function fillThumbnail(img, metarecord) {
  try {
    const paths = await daemon.metarecordPaths(repo, metarecord);
    if (paths.length > 0) img.src = `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(paths[0])}`;
  } catch {
    /* no preview */
  }
}

// ── Selection (workspace variables) ─────────────────────────────────────

async function setCursor(index) {
  cursorIndex = Math.max(0, Math.min(index, metarecords.length - 1));
  render();
  const metarecord = metarecords[cursorIndex];
  if (!metarecord) return;
  document.querySelector('tr.cursor')?.scrollIntoView({ block: 'nearest' });
  await workspace.set('selected_metarecord', { uuid: metarecord.uuid, repo });
  await workspace.set('selected_paths', await daemon.metarecordPaths(repo, metarecord));
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
  const paths = await daemon.metarecordPaths(repo, metarecord);
  await commands.invoke(`panel:reveal-other ${paths.length > 0 ? 'file' : 'metarecord-detail'}`);
}

// ── Query (two-zone editor) ─────────────────────────────────────────────
//
// Zone A (#query-input) holds the simplified query; zone B (#normal-input)
// the normal DSL. When B is shown and not frozen it is a read-only live mirror
// of expand(A); the Freeze checkbox decouples it so it becomes editable and
// authoritative. The query that runs is always B's content (computed as
// expand(A) when B is hidden). See spec-gui "Query editor (two zones)".

/** Recomputes `queryIR` from the current editor state (no fetch). Returns true
 *  on success; on error fills the relevant slot and leaves `queryIR` as-is. */
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
      // The offending DSL is visible in B when shown; otherwise show under A.
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
  // Unfreezing re-syncs B from A (manual edits discarded — accepted).
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
  render();
  // Persisted per workspace; also lets scripts set the columns.
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

document
  .getElementById('query-apply')
  .addEventListener('click', () => void commands.invoke('metarecord-list:apply-query'));
document
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

await metafolder.ready;

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
    document.body.classList.toggle('grid', mode === 'grid');
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
    // The var-change listener applies it (single code path, also for scripts).
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
  document.getElementById('no-repo').hidden = repo !== null;
  if (!queryInitialized) {
    // First display: compile the (possibly restored) query once. Kept off the
    // construction path so a hidden pre-instantiated panel makes no daemon call.
    queryInitialized = true;
    await recomputeQuery();
  }
  if (repo !== null) await fetchPage(true);
}

// The first query waits for the first actual display: construction stays
// cheap so the panel type can be pre-instantiated hidden (commands
// registered, no daemon traffic). start() re-reads the repo and fetches
// with the selection preserved, so one stable callback covers every
// trigger; re-arming while hidden coalesces into a single run on show.
const deferredStart = () => void start();
// Refresh when metarecord-detail (or another panel) writes metarecords.
workspace.onChange('metarecords:dirty', () => metafolder.whenVisible(deferredStart));
// The workspace may adopt a repo after startup (repos panel).
workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
// Columns may also be set by a script (mf gui) or another session.
workspace.onChange('metarecord-list:columns', (value) => {
  setColumns(value);
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

// Restore the two-zone query editor (values only — no daemon call here; the
// first expand/parse happens in start() on the first display).
queryInput.value = (await workspace.get('metarecord-list:query')) ?? '';
normalInput.value = (await workspace.get('metarecord-list:normal-query')) ?? '';
normalFrozen = (await workspace.get('metarecord-list:normal-frozen')) ?? false;
normalFreeze.checked = normalFrozen;
normalInput.readOnly = !normalFrozen;
normalShown = (await workspace.get('metarecord-list:normal-shown')) ?? false;
normalEditor.hidden = !normalShown;
normalToggle.textContent = normalShown ? 'Hide normal DSL' : 'Show normal DSL';

metafolder.whenVisible(deferredStart);
