// record-list panel: records of the active repo filtered by an embedded
// DSL query; primary selection source (spec-gui "record-list panel type").

import { el } from '/__ui.js';
import { parseColumns, isSortable, cellQuickText, cellText } from './columns.js';

const { daemon, workspace, commands, statusBar } = metafolder;

const PAGE_SIZE = 100;
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
let records = [];
let nextCursor = null;
let loading = false;
let queryIR = null; // null = match all
let sort = []; // [{field, order}]
let cursorIndex = -1;
let checked = new Set(); // multi-selection (uuids)
let mode = 'table';
let refCache = new Map(); // uuid -> Promise<record>, for ~target columns

const rows = document.getElementById('rows');
const grid = document.getElementById('grid');
const scroll = document.getElementById('scroll');
const statusLine = document.getElementById('status-line');
const queryInput = document.getElementById('query-input');
const columnsInput = document.getElementById('columns-input');
const queryError = document.getElementById('query-error');
const columnsError = document.getElementById('columns-error');

// ── Data access ─────────────────────────────────────────────────────────

// Context for ~ column display (columns.js); the ref target cache lives
// for one result set and is dropped on every reset fetch.
const displayCtx = {
  resolveTreeRef: (value) => daemon.resolveTreeRef(repo, value),
  getRecord: (uuid) => {
    if (!refCache.has(uuid)) {
      refCache.set(uuid, daemon.call('GET', `/repos/${repo}/records/${uuid}`));
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
      // A refresh (new query, records:dirty, …) must not steal the
      // selection: remember the highlighted record and restore it.
      keepUuid =
        records[cursorIndex]?.uuid ?? (await workspace.get('selected_record'))?.uuid ?? null;
      records = [];
      nextCursor = null;
      refCache = new Map();
    }
    let page;
    try {
      page = await daemon.call('POST', `/repos/${repo}/query`, {
        query: queryIR ?? MATCH_ALL,
        select: '*',
        limit: PAGE_SIZE,
        ...(sort.length > 0 && { sort }),
        ...(nextCursor && { cursor: nextCursor }),
      });
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    records = records.concat(page.results);
    nextCursor = page.next_cursor;
    if (reset) {
      // Drop checked records that no longer match.
      const alive = new Set(records.map((e) => e.uuid));
      if ([...checked].some((uuid) => !alive.has(uuid))) {
        checked = new Set([...checked].filter((uuid) => alive.has(uuid)));
        await workspace.set('selected_records', [...checked]);
      }
      const keepIndex = keepUuid === null ? -1 : records.findIndex((e) => e.uuid === keepUuid);
      if (keepIndex >= 0) {
        // Same record still listed: move the cursor back silently, the
        // selection variables are already correct.
        cursorIndex = keepIndex;
      } else if (records.length > 0) {
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
  if (moved) void workspace.set('record-list:column-widths', { ...widths });
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

function fillCell(node, column, record) {
  cellText(column, record, displayCtx).then(
    (text) => (node.textContent = text),
    () => {}, // keep the quick text
  );
}

function render() {
  renderHeader();
  rows.replaceChildren(
    ...records.map((record, index) =>
      el(
        'tr',
        {
          class: ['row', index === cursorIndex && 'cursor', checked.has(record.uuid) && 'checked'],
          onclick: () => setCursor(index),
          ondblclick: () => openSelected(),
        },
        columns.map((column) => {
          const td = el('td', {}, cellQuickText(column, record));
          if (column.deref !== null) fillCell(td, column, record);
          return td;
        }),
      ),
    ),
  );
  grid.replaceChildren(
    ...records.map((record, index) => {
      const img = el('img', { loading: 'lazy' });
      void fillThumbnail(img, record);
      return el(
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
          cellQuickText(GRID_NAME_COLUMN, record) || record.uuid.slice(0, 8),
        ),
      );
    }),
  );
  statusLine.textContent =
    `${records.length} record${records.length === 1 ? '' : 's'}` +
    (nextCursor ? ' (more available — scroll down)' : '') +
    (checked.size > 0 ? ` — ${checked.size} selected` : '');
}

async function fillThumbnail(img, record) {
  try {
    const paths = await daemon.recordPaths(repo, record);
    if (paths.length > 0) img.src = `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(paths[0])}`;
  } catch {
    /* no preview */
  }
}

// ── Selection (workspace variables) ─────────────────────────────────────

async function setCursor(index) {
  cursorIndex = Math.max(0, Math.min(index, records.length - 1));
  render();
  const record = records[cursorIndex];
  if (!record) return;
  document.querySelector('tr.cursor')?.scrollIntoView({ block: 'nearest' });
  await workspace.set('selected_record', { uuid: record.uuid, repo });
  await workspace.set('selected_paths', await daemon.recordPaths(repo, record));
}

async function toggleChecked() {
  const record = records[cursorIndex];
  if (!record) return;
  if (checked.has(record.uuid)) checked.delete(record.uuid);
  else checked.add(record.uuid);
  render();
  await workspace.set('selected_records', [...checked]);
}

async function openSelected() {
  const record = records[cursorIndex];
  if (!record) return;
  const paths = await daemon.recordPaths(repo, record);
  await commands.invoke(`panel:reveal-other ${paths.length > 0 ? 'file' : 'record-detail'}`);
}

// ── Query and sort ──────────────────────────────────────────────────────

async function applyQuery() {
  queryError.textContent = '';
  const dsl = queryInput.value.trim();
  if (dsl === '') {
    queryIR = null;
  } else {
    try {
      queryIR = await daemon.parseQuery(dsl);
    } catch (error) {
      queryError.textContent = String(error.message ?? error);
      return;
    }
  }
  await fetchPage(true);
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
  await workspace.set('record-list:columns', columns.map((c) => c.spec));
}

function toggleSort(column) {
  if (!isSortable(column)) return; // record meta, not a sortable field
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

document.getElementById('query-apply').addEventListener('click', () => void applyQuery());
document.getElementById('columns-apply').addEventListener('click', () => void applyColumns());
queryInput.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') void applyQuery();
});
columnsInput.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') void applyColumns();
});

// ── Wiring ──────────────────────────────────────────────────────────────

await metafolder.ready;

commands.register('record-list:next', {
  label: 'Record list: move the selection down',
  handler: () => setCursor(cursorIndex + 1),
});
commands.register('record-list:prev', {
  label: 'Record list: move the selection up',
  handler: () => setCursor(cursorIndex - 1),
});
commands.register('record-list:select-toggle', {
  label: 'Record list: toggle multi-selection',
  handler: toggleChecked,
});
commands.register('record-list:open', {
  label: 'Record list: open the selection in the other panel',
  handler: openSelected,
});
commands.register('record-list:set-mode', {
  label: 'Record list: switch display mode (table | grid)',
  handler: (newMode) => {
    mode = newMode === 'grid' ? 'grid' : 'table';
    document.body.classList.toggle('grid', mode === 'grid');
  },
});
commands.register('record-list:edit-query', {
  label: 'Record list: focus the query input',
  handler: () => queryInput.focus(),
});
commands.register('record-list:edit-columns', {
  label: 'Record list: focus the columns input',
  handler: () => columnsInput.focus(),
});
commands.register('record-list:refresh', {
  label: 'Record list: reload from the daemon',
  handler: () => fetchPage(true),
});

metafolder.addKeybinding('record-list:next', 'down');
metafolder.addKeybinding('record-list:next', 'j');
metafolder.addKeybinding('record-list:prev', 'up');
metafolder.addKeybinding('record-list:prev', 'k');
metafolder.addKeybinding('record-list:select-toggle', 'space');
metafolder.addKeybinding('record-list:open', 'enter');
metafolder.addKeybinding('record-list:open', 'right');
metafolder.addKeybinding('record-list:edit-query', '/');

async function start() {
  repo = await workspace.get('active_repo');
  document.getElementById('no-repo').hidden = repo !== null;
  if (repo !== null) await fetchPage(true);
}

// Refresh when record-detail (or another panel) writes records.
workspace.onChange('records:dirty', () => void fetchPage(true));
// The workspace may adopt a repo after startup (repos panel).
workspace.onChange('active_repo', () => void start());
// Columns may also be set by a script (mf gui) or another session.
workspace.onChange('record-list:columns', (value) => {
  setColumns(value);
  render();
});

setColumns(await workspace.get('record-list:columns'));
widths = (await workspace.get('record-list:column-widths')) ?? {};
await start();
