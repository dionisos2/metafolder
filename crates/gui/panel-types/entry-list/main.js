// entry-list panel: entries of the active repo filtered by an embedded
// DSL query; primary selection source (spec-gui "entry-list panel type").

const { daemon, workspace, commands, statusBar } = metafolder;

const PAGE_SIZE = 100;
const COLUMNS = ['mfr_path', 'mfr_type', 'version'];

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
let repoRoot = null;
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
const queryEditor = document.getElementById('query-editor');
const queryInput = document.getElementById('query-input');
const queryError = document.getElementById('query-error');

// ── Data access ─────────────────────────────────────────────────────────

function field(entry, name) {
  return (entry.fields ?? []).find((f) => f.name === name);
}

function fieldDisplay(entry, name) {
  const f = field(entry, name);
  if (!f) return '';
  const { type, value } = f.value;
  if (type === 'tree_ref') return value.name || '(root)';
  if (type === 'nothing') return '∅';
  return String(value);
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
    const body = {
      query: queryIR ?? MATCH_ALL,
      select: '*',
      limit: PAGE_SIZE,
      ...(sort.length > 0 && { sort }),
      ...(nextCursor && { cursor: nextCursor }),
    };
    const response = await daemon.request('POST', `/repos/${repo}/query`, body);
    if (response.status !== 200) {
      statusBar.message(response.body?.error ?? `query failed (${response.status})`);
      return;
    }
    entries = entries.concat(response.body.results);
    nextCursor = response.body.next_cursor;
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
  const header = document.getElementById('header-row');
  header.replaceChildren(
    ...COLUMNS.map((name) => {
      const th = document.createElement('th');
      const active = sort.find((s) => s.field === name);
      th.textContent = name + (active ? (active.order === 'asc' ? ' ▲' : ' ▼') : '');
      th.addEventListener('click', () => toggleSort(name));
      return th;
    }),
  );
}

function render() {
  renderHeader();
  rows.replaceChildren(
    ...entries.map((entry, index) => {
      const tr = document.createElement('tr');
      tr.className = 'row';
      tr.classList.toggle('cursor', index === cursorIndex);
      tr.classList.toggle('checked', checked.has(entry.uuid));
      for (const column of COLUMNS) {
        const td = document.createElement('td');
        td.textContent =
          column === 'version' ? String(entry.version) : fieldDisplay(entry, column);
        if (column === 'mfr_path') {
          td.dataset.uuid = entry.uuid;
          void fillResolvedPath(td, entry);
        }
        tr.appendChild(td);
      }
      tr.addEventListener('click', () => setCursor(index));
      tr.addEventListener('dblclick', () => openSelected());
      return tr;
    }),
  );
  grid.replaceChildren(
    ...entries.map((entry, index) => {
      const card = document.createElement('div');
      card.className = 'card';
      card.classList.toggle('cursor', index === cursorIndex);
      const img = document.createElement('img');
      img.loading = 'lazy';
      void fillThumbnail(img, entry);
      const name = document.createElement('div');
      name.className = 'name';
      name.textContent = fieldDisplay(entry, 'mfr_path') || entry.uuid.slice(0, 8);
      card.append(img, name);
      card.addEventListener('click', () => setCursor(index));
      card.addEventListener('dblclick', () => openSelected());
      return card;
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
    const paths = await resolvedAbsolutePaths(entry);
    if (paths.length > 0) img.src = `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(paths[0])}`;
  } catch {
    /* no preview */
  }
}

// ── Selection (workspace variables) ─────────────────────────────────────

async function resolvedAbsolutePaths(entry) {
  const root = repoRoot ?? (repoRoot = await daemon.repoRoot(repo));
  const refs = (entry.fields ?? []).filter(
    (f) => f.name === 'mfr_path' && f.value.type === 'tree_ref',
  );
  const paths = [];
  for (const f of refs) {
    try {
      const relative = await daemon.resolveTreeRef(repo, f.value.value);
      paths.push(relative === '' ? root : `${root}/${relative}`);
    } catch {
      /* unresolvable (stale) path: skip */
    }
  }
  return paths;
}

async function setCursor(index) {
  cursorIndex = Math.max(0, Math.min(index, entries.length - 1));
  render();
  const entry = entries[cursorIndex];
  if (!entry) return;
  document.querySelector('tr.cursor')?.scrollIntoView({ block: 'nearest' });
  await workspace.set('selected_entry', { uuid: entry.uuid, repo });
  await workspace.set('selected_paths', await resolvedAbsolutePaths(entry));
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
  const paths = await resolvedAbsolutePaths(entry);
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

document.getElementById('query-apply').addEventListener('click', applyQuery);
queryInput.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') void applyQuery();
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
  label: 'Entry list: toggle the query editor',
  handler: () => {
    queryEditor.classList.toggle('open');
    if (queryEditor.classList.contains('open')) queryInput.focus();
  },
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

await start();
