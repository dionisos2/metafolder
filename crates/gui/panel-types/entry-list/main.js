// entry-list panel: entries of the active repo filtered by an embedded
// DSL query; primary selection source (spec-gui "entry-list panel type").

import { el, field, fields, formatValue } from '/__ui.js';

const { daemon, workspace, commands, statusBar } = metafolder;

const PAGE_SIZE = 100;
const DEFAULT_COLUMNS = ['mfr_path', 'mfr_type', 'version'];

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
let columns = DEFAULT_COLUMNS; // shown as table columns; persisted per workspace
let entries = [];
let nextCursor = null;
let loading = false;
let queryIR = null; // null = match all
let sort = []; // [{field, order}]
let cursorIndex = -1;
let checked = new Set(); // multi-selection (uuids)
let mode = 'table';

const rows = document.getElementById('rows');
const grid = document.getElementById('grid');
const scroll = document.getElementById('scroll');
const statusLine = document.getElementById('status-line');
const queryInput = document.getElementById('query-input');
const columnsInput = document.getElementById('columns-input');
const queryError = document.getElementById('query-error');

// ── Data access ─────────────────────────────────────────────────────────

function fieldDisplay(entry, name) {
  // Multi-map: a column cell shows every row of the field.
  return fields(entry, name)
    .map((f) =>
      // mfr_path column: leaf name only, until the resolved path arrives.
      f.value.type === 'tree_ref' ? f.value.value.name || '(root)' : formatValue(f.value),
    )
    .join(', ');
}

async function fetchPage(reset) {
  if (!repo || loading) return;
  loading = true;
  try {
    let keepUuid = null;
    if (reset) {
      // A refresh (new query, entries:dirty, …) must not steal the
      // selection: remember the highlighted entry and restore it.
      keepUuid =
        entries[cursorIndex]?.uuid ?? (await workspace.get('selected_entry'))?.uuid ?? null;
      entries = [];
      nextCursor = null;
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
    entries = entries.concat(page.results);
    nextCursor = page.next_cursor;
    if (reset) {
      // Drop checked entries that no longer match.
      const alive = new Set(entries.map((e) => e.uuid));
      if ([...checked].some((uuid) => !alive.has(uuid))) {
        checked = new Set([...checked].filter((uuid) => alive.has(uuid)));
        await workspace.set('selected_entries', [...checked]);
      }
      const keepIndex = keepUuid === null ? -1 : entries.findIndex((e) => e.uuid === keepUuid);
      if (keepIndex >= 0) {
        // Same entry still listed: move the cursor back silently, the
        // selection variables are already correct.
        cursorIndex = keepIndex;
      } else if (entries.length > 0) {
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

function renderHeader() {
  document.getElementById('header-row').replaceChildren(
    ...columns.map((name) => {
      const active = sort.find((s) => s.field === name);
      return el(
        'th',
        { onclick: () => toggleSort(name) },
        name + (active ? (active.order === 'asc' ? ' ▲' : ' ▼') : ''),
      );
    }),
  );
}

function render() {
  renderHeader();
  rows.replaceChildren(
    ...entries.map((entry, index) =>
      el(
        'tr',
        {
          class: ['row', index === cursorIndex && 'cursor', checked.has(entry.uuid) && 'checked'],
          onclick: () => setCursor(index),
          ondblclick: () => openSelected(),
        },
        columns.map((column) => {
          const td = el(
            'td',
            {},
            column === 'version' ? String(entry.version) : fieldDisplay(entry, column),
          );
          if (column === 'mfr_path') {
            td.dataset.uuid = entry.uuid;
            void fillResolvedPath(td, entry);
          }
          return td;
        }),
      ),
    ),
  );
  grid.replaceChildren(
    ...entries.map((entry, index) => {
      const img = el('img', { loading: 'lazy' });
      void fillThumbnail(img, entry);
      return el(
        'div',
        {
          class: ['card', index === cursorIndex && 'cursor'],
          onclick: () => setCursor(index),
          ondblclick: () => openSelected(),
        },
        img,
        el('div', { class: 'name' }, fieldDisplay(entry, 'mfr_path') || entry.uuid.slice(0, 8)),
      );
    }),
  );
  statusLine.textContent =
    `${entries.length} entr${entries.length === 1 ? 'y' : 'ies'}` +
    (nextCursor ? ' (more available — scroll down)' : '') +
    (checked.size > 0 ? ` — ${checked.size} selected` : '');
}

async function fillResolvedPath(td, entry) {
  const f = field(entry, 'mfr_path');
  if (!f || f.value.type !== 'tree_ref') return;
  try {
    td.textContent = await daemon.resolveTreeRef(repo, f.value.value);
  } catch {
    /* stale TreeRef: keep the name component */
  }
}

async function fillThumbnail(img, entry) {
  try {
    const paths = await daemon.entryPaths(repo, entry);
    if (paths.length > 0) img.src = `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(paths[0])}`;
  } catch {
    /* no preview */
  }
}

// ── Selection (workspace variables) ─────────────────────────────────────

async function setCursor(index) {
  cursorIndex = Math.max(0, Math.min(index, entries.length - 1));
  render();
  const entry = entries[cursorIndex];
  if (!entry) return;
  document.querySelector('tr.cursor')?.scrollIntoView({ block: 'nearest' });
  await workspace.set('selected_entry', { uuid: entry.uuid, repo });
  await workspace.set('selected_paths', await daemon.entryPaths(repo, entry));
}

async function toggleChecked() {
  const entry = entries[cursorIndex];
  if (!entry) return;
  if (checked.has(entry.uuid)) checked.delete(entry.uuid);
  else checked.add(entry.uuid);
  render();
  await workspace.set('selected_entries', [...checked]);
}

async function openSelected() {
  const entry = entries[cursorIndex];
  if (!entry) return;
  const paths = await daemon.entryPaths(repo, entry);
  await commands.invoke(`panel:reveal-other ${paths.length > 0 ? 'file' : 'entry-detail'}`);
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
  columns = Array.isArray(value) && value.length > 0 ? value : DEFAULT_COLUMNS;
  columnsInput.value = columns.join(' ');
}

/** Applies the columns input (no daemon round-trip: select is always '*'). */
async function applyColumns() {
  const parsed = columnsInput.value.trim().split(/[\s,]+/).filter(Boolean);
  setColumns(parsed);
  render();
  // Persisted per workspace; also lets scripts set the columns.
  await workspace.set('entry-list:columns', columns);
}

function toggleSort(column) {
  if (column === 'version') return; // entry meta, not a sortable field
  const current = sort.find((s) => s.field === column);
  sort = current
    ? current.order === 'asc'
      ? [{ field: column, order: 'desc' }]
      : []
    : [{ field: column, order: 'asc' }];
  void fetchPage(true);
}

scroll.addEventListener('scroll', () => {
  if (nextCursor && scroll.scrollTop + scroll.clientHeight > scroll.scrollHeight - 200) {
    void fetchPage(false);
  }
});

document.getElementById('query-apply').addEventListener('click', () => {
  void applyColumns();
  void applyQuery();
});
queryInput.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') void applyQuery();
});
columnsInput.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') void applyColumns();
});

// ── Wiring ──────────────────────────────────────────────────────────────

await metafolder.ready;

commands.register('entry-list:next', {
  label: 'Entry list: move the selection down',
  handler: () => setCursor(cursorIndex + 1),
});
commands.register('entry-list:prev', {
  label: 'Entry list: move the selection up',
  handler: () => setCursor(cursorIndex - 1),
});
commands.register('entry-list:select-toggle', {
  label: 'Entry list: toggle multi-selection',
  handler: toggleChecked,
});
commands.register('entry-list:open', {
  label: 'Entry list: open the selection in the other panel',
  handler: openSelected,
});
commands.register('entry-list:set-mode', {
  label: 'Entry list: switch display mode (table | grid)',
  handler: (newMode) => {
    mode = newMode === 'grid' ? 'grid' : 'table';
    document.body.classList.toggle('grid', mode === 'grid');
  },
});
commands.register('entry-list:edit-query', {
  label: 'Entry list: focus the query input',
  handler: () => queryInput.focus(),
});
commands.register('entry-list:edit-columns', {
  label: 'Entry list: focus the columns input',
  handler: () => columnsInput.focus(),
});
commands.register('entry-list:refresh', {
  label: 'Entry list: reload from the daemon',
  handler: () => fetchPage(true),
});

metafolder.addKeybinding('entry-list:next', 'down');
metafolder.addKeybinding('entry-list:next', 'j');
metafolder.addKeybinding('entry-list:prev', 'up');
metafolder.addKeybinding('entry-list:prev', 'k');
metafolder.addKeybinding('entry-list:select-toggle', 'space');
metafolder.addKeybinding('entry-list:open', 'enter');
metafolder.addKeybinding('entry-list:open', 'right');
metafolder.addKeybinding('entry-list:edit-query', '/');

async function start() {
  repo = await workspace.get('active_repo');
  document.getElementById('no-repo').hidden = repo !== null;
  if (repo !== null) await fetchPage(true);
}

// Refresh when entry-detail (or another panel) writes entries.
workspace.onChange('entries:dirty', () => void fetchPage(true));
// The workspace may adopt a repo after startup (repos panel).
workspace.onChange('active_repo', () => void start());
// Columns may also be set by a script (mf gui) or another session.
workspace.onChange('entry-list:columns', (value) => {
  setColumns(value);
  render();
});

setColumns(await workspace.get('entry-list:columns'));
await start();
