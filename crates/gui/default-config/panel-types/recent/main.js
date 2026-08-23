// recent panel: the repository's recently-viewed metarecords (crate::recent),
// newest first, one row "<name>  <mfr_path>  <viewed N ago>". A GUI-side
// concern — the list comes from metafolder.recent (no daemon endpoint); the
// display fields (label/name/mfr_path) are read from the shared cache. Opening
// a row publishes the selection and reveals the viewer, like the metarecord
// list. The list is a snapshot: it reloads on repo change, on a metarecord
// change, or on an explicit refresh — not on every view — so it never reorders
// under the cursor while you navigate.

import { byId, el, field, formatValue } from '/__ui.js';
import { rowActionsProvider, baseName } from '/__file-actions.js';

/** The first field named `name` on `rec` as text, or '' when absent.
 *  @param {Metafolder.Metarecord} rec @param {string} name */
function fieldText(rec, name) {
  const f = field(rec, name);
  return f ? formatValue(f.value) : '';
}

/** Coarse "how long ago" from an ISO-8601 timestamp, or '' if unparseable.
 *  @param {string} iso @param {number} [now] */
export function formatViewedAge(iso, now = Date.now()) {
  const ms = Date.parse(iso);
  if (Number.isNaN(ms)) return '';
  const secs = Math.max(0, Math.floor((now - ms) / 1000));
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

/**
 * @typedef {{ uuid: string, viewedAt: string, label: string, name: string,
 *   relPath: string, absPaths: string[], isDir: boolean }} Row
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, cache } = metafolder;
  /** @type {Metafolder.Refresh} */
  const REFRESH = cache.REFRESH;

  /** @type {string|null} */
  let repo = null;
  /** @type {Row[]} */
  let rows = [];
  let cursorIndex = -1;

  const entriesList = byId(root, 'entries');
  const placeholder = byId(root, 'placeholder');
  const statusLine = byId(root, 'status-line');

  function selected() {
    return rows[cursorIndex] ?? null;
  }

  /** A row's display name: its label, else its filename, else a short uuid.
   *  @param {Row} row */
  function rowName(row) {
    return row.label || row.name || `${row.uuid.slice(0, 8)}…`;
  }

  function render() {
    const now = Date.now();
    placeholder.hidden = rows.length > 0;
    entriesList.hidden = rows.length === 0;
    if (repo === null) placeholder.textContent = 'No active repository.';
    else if (rows.length === 0) placeholder.textContent = 'No metarecords viewed yet.';

    entriesList.replaceChildren(
      ...rows.map((row, index) =>
        el(
          'li',
          {
            class: [index === cursorIndex && 'cursor'],
            onclick: () => select(index),
            ondblclick: () => void open(),
            'data-mf-uuid': row.uuid,
            ...(row.absPaths[0]
              ? {
                  'data-mf-path': row.absPaths[0],
                  'data-mf-isdir': row.isDir ? '1' : '0',
                  'data-mf-name': row.name || baseName(row.absPaths[0]),
                }
              : {}),
          },
          el('span', { class: 'name' }, rowName(row)),
          el('span', { class: 'path' }, row.relPath),
          el('span', { class: 'age' }, formatViewedAge(row.viewedAt, now)),
        ),
      ),
    );
    statusLine.textContent = rows.length === 1 ? '1 metarecord' : `${rows.length} metarecords`;
  }

  /** Moves the cursor and publishes the selection so a paired detail/file panel
   *  follows (the same live-preview the metarecord list does).
   *  @param {number} index */
  async function select(index) {
    cursorIndex = Math.max(0, Math.min(index, rows.length - 1));
    render();
    root.querySelector('li.cursor')?.scrollIntoView({ block: 'nearest' });
    const row = selected();
    if (!row || !repo) return;
    await workspace.set('selected_metarecord', { uuid: row.uuid, repo });
    await workspace.set('selected_paths', row.absPaths);
  }

  /** Opens the highlighted row in the other slot (file when it has paths, else
   *  metarecord-detail), after publishing the selection. */
  async function open() {
    const row = selected();
    if (!row || !repo) return;
    await workspace.set('selected_metarecord', { uuid: row.uuid, repo });
    await workspace.set('selected_paths', row.absPaths);
    await commands.invoke(`panel:reveal-other ${row.absPaths.length > 0 ? 'file' : 'metarecord-detail'}`);
  }

  async function load() {
    const r = repo;
    if (r === null) {
      rows = [];
      render();
      return;
    }
    let entries;
    try {
      entries = await metafolder.recent.list(r);
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    const uuids = entries.map((e) => e.uuid);
    const root_path = await daemon.repoRoot(r).catch(() => null);
    await Promise.all([
      cache.fetchMetarecords(r, uuids),
      cache.fetchTreeRefs(r, 'mfr_path', uuids),
    ]);
    rows = entries.map((e) => {
      const rec = cache.readMetarecord(r, e.uuid);
      const rel = cache.readTreeRef(r, 'mfr_path', e.uuid);
      const relPaths = rel === REFRESH ? [] : rel;
      const absPaths =
        root_path === null ? [] : relPaths.map((p) => (p === '' ? root_path : `${root_path}${p}`));
      return {
        uuid: e.uuid,
        viewedAt: e.viewed_at,
        label: rec === REFRESH ? '' : fieldText(rec, 'label'),
        name: rec === REFRESH ? '' : fieldText(rec, 'name'),
        relPath: relPaths[0] ?? '',
        absPaths,
        isDir: rec !== REFRESH && fieldText(rec, 'mfr_type') === 'dir',
      };
    });
    if (cursorIndex >= rows.length) cursorIndex = rows.length - 1;
    render();
  }

  byId(root, 'refresh').addEventListener('click', () => void load());

  void commands.register('recent:refresh', {
    label: 'Recent: reload the list',
    handler: () => load(),
  });
  void commands.register('recent:next', {
    label: 'Recent: move down',
    handler: () => select(cursorIndex + 1),
  });
  void commands.register('recent:prev', {
    label: 'Recent: move up',
    handler: () => select(cursorIndex - 1),
  });
  void commands.register('recent:first', {
    label: 'Recent: jump to the most recent',
    handler: () => select(0),
  });
  void commands.register('recent:last', {
    label: 'Recent: jump to the oldest',
    handler: () => select(rows.length - 1),
  });
  void commands.register('recent:open', {
    label: 'Recent: open the highlighted metarecord in the other panel',
    handler: () => open(),
  });

  // Keybindings for this panel live in keybindings.toml (when = "recent").

  // Right-click a row: the shared "Metarecord" section (open in detail/file,
  // reveal folder, Copy UUID) plus, when the record is file-backed, the file
  // actions (cut/copy/paste/rename/duplicate/trash) — see /__file-actions.js.
  metafolder.contextMenu.addDefaultItems(rowActionsProvider(metafolder, () => repo));

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    await load();
  }

  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  // A view (touch) never sets metarecords:dirty, so the snapshot is stable while
  // navigating; a real metarecord change (rename, delete, rollback) does, and
  // refreshes the display fields.
  workspace.onChange('metarecords:dirty', () => {
    if (repo !== null) void load();
  });
  metafolder.whenVisible(deferredStart);
}
