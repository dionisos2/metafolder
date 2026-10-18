// recent panel: the repository's recently-viewed metarecords (crate::recent),
// newest first — or best first by score, `recent:set sort` (doc "Frecent
// folders") — one row "<name>  <mfr_path>  <score>  <viewed N ago>". A GUI-side
// concern — the list comes from metafolder.recent (no daemon endpoint); the
// display fields (label/name/mfr_path) are read from the daemon. Opening
// a row publishes the selection and reveals the viewer, like the metarecord
// list. The list is a snapshot: it reloads on repo change, on a metarecord
// change, or on an explicit refresh — not on every view — so it never reorders
// under the cursor while you navigate.

import {
  byId,
  el,
  field,
  formatAge,
  formatScore,
  formatValue,
  frecency,
  viewerButtons,
} from '/__ui.js';
import { createSelect } from '/__select.js';
import { createListCursor } from '/__list-cursor.js';
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
  return Number.isNaN(ms) ? '' : formatAge(ms, now);
}

/**
 * @typedef {{ uuid: string, viewedAt: string, score: number, label: string, name: string,
 *   relPath: string, absPaths: string[], isDir: boolean }} Row
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar } = metafolder;

  /** @type {string|null} */
  let repo = null;
  /** @type {Row[]} */
  let rows = [];
  let cursorIndex = -1;
  /** The row order: `time` (newest first, as the list is kept) or `score`
   *  (best first). The workspace's `recent:sort`, so it survives a restart. */
  let sort = 'time';
  /** @type {Row[]} the rows as the list keeps them, newest first */
  let byTime = [];

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
            'data-mf-uuid': row.uuid,
            ...(row.absPaths[0]
              ? {
                  'data-mf-path': row.absPaths[0],
                  'data-mf-isdir': row.isDir ? '1' : '0',
                  'data-mf-name': row.name || baseName(row.absPaths[0]),
                }
              : {}),
          },
          // doc "Opening a row"
          viewerButtons({
            hasFile: row.absPaths.length > 0,
            hasMetarecord: true,
            open: async (what) => {
              await select(index);
              await commands.invoke(['panel:open', 'other', what]);
            },
          }),
          el('span', { class: 'name' }, rowName(row)),
          el('span', { class: 'path' }, row.relPath),
          el(
            'span',
            { class: 'score', title: 'Views, weighted by how recent (doc "Frecent folders")' },
            `score ${formatScore(row.score)}`,
          ),
          el('span', { class: 'age' }, formatViewedAge(row.viewedAt, now)),
        ),
      ),
    );
    statusLine.textContent = rows.length === 1 ? '1 metarecord' : `${rows.length} metarecords`;
  }

  // The rows are index-aligned with `rows`: moving the cursor retargets the
  // highlight, and the selection is published coalesced (doc "Moving the
  // cursor").
  const cursor = createListCursor({
    containers: () => [entriesList],
    publish: async () => {
      const row = selected();
      if (!row || !repo) return;
      await workspace.set('selected_metarecord', { uuid: row.uuid, repo });
      await workspace.set('selected_paths', row.absPaths);
    },
  });

  /** Moves the cursor and publishes the selection so a paired detail/file panel
   *  follows (the same live-preview the metarecord list does).
   *  @param {number} index */
  async function select(index) {
    cursorIndex = Math.max(0, Math.min(index, rows.length - 1));
    await cursor.move(cursorIndex);
  }

  async function load() {
    const r = repo;
    if (r === null) {
      rows = [];
      byTime = [];
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
    // A failed read leaves rows without their name or path, never without rows.
    const [records, paths] = await Promise.all([
      daemon.metarecords(r, uuids).catch(() => new Map()),
      daemon.treePaths(r, 'mfr_path', uuids).catch(() => ({})),
    ]);
    const now = Date.now();
    byTime = entries.map((e) => {
      const rec = records.get(e.uuid);
      const relPaths = /** @type {Record<string, string[]>} */ (paths)[e.uuid] ?? [];
      const absPaths =
        root_path === null ? [] : relPaths.map((p) => (p === '' ? root_path : `${root_path}${p}`));
      return {
        uuid: e.uuid,
        viewedAt: e.viewed_at,
        score: frecency(e.rank, e.viewed_at, now),
        label: rec === undefined ? '' : fieldText(rec, 'label'),
        name: rec === undefined ? '' : fieldText(rec, 'name'),
        relPath: relPaths[0] ?? '',
        absPaths,
        isDir: rec !== undefined && fieldText(rec, 'mfr_type') === 'dir',
      };
    });
    arrange();
  }

  /** Orders `rows` by `sort`, keeping the cursor on the row it was on. */
  function arrange() {
    const at = selected()?.uuid;
    rows = sort === 'score' ? [...byTime].sort((a, b) => b.score - a.score) : byTime;
    const index = at === undefined ? -1 : rows.findIndex((row) => row.uuid === at);
    cursorIndex = index >= 0 ? index : Math.min(cursorIndex, rows.length - 1);
    render();
  }

  const SORTS = ['time', 'score'];
  const sortSelect = createSelect(byId(root, 'sort'), {
    value: sort,
    options: [
      { value: 'time', label: 'newest first' },
      { value: 'score', label: 'best score first' },
    ],
    onChange: (value) => void setSort(value),
  });

  /** @param {string} value */
  function setSort(value) {
    if (!SORTS.includes(value)) throw new Error(`unknown order: "${value}" (time / score)`);
    sort = value;
    sortSelect.setValue(sort);
    arrange();
    return workspace.set('recent:sort', sort);
  }

  byId(root, 'refresh').addEventListener('click', () => void load());

  // Read again on `view:refresh` (doc "Generic panel commands").
  metafolder.provide({ refresh: () => load() });
  // The cursor, for the generic commands (doc "Generic panel commands").
  // Several recently-viewed records can share a display name, so each find
  // candidate carries its path, which the typed terms search too.
  metafolder.provide({
    index: () => cursorIndex,
    count: () => rows.length,
    select,
    entries: () =>
      rows.map((row) => ({
        name: rowName(row),
        ...(row.relPath && { label: `${rowName(row)} — ${row.relPath}` }),
      })),
  });
  void commands.register('recent:set', {
    label: 'Recent: change a view setting (sort: time / score)',
    args: [
      { name: 'setting', prompt: () => 'Which setting? (sort)', complete: () => ['sort'] },
      {
        name: 'value',
        prompt: () => 'Order? (time / score)',
        // The other order, so the key toggles with one Enter.
        initial: () => (sort === 'time' ? 'score' : 'time'),
        complete: () => SORTS,
      },
    ],
    handler: (setting, value) => {
      if (setting !== 'sort') throw new Error(`unknown setting: "${setting ?? ''}" (sort)`);
      return setSort(value);
    },
  });
  // Keybindings for this panel live in keybindings.toml (when = "recent").

  // Right-click a row: the shared "Metarecord" section (open in detail/file,
  // reveal folder, Copy UUID) plus, when the record is file-backed, the file
  // actions (cut/copy/paste/rename/duplicate/trash) — see /__file-actions.js.
  metafolder.contextMenu.addDefaultItems(rowActionsProvider(metafolder, () => repo));

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    const saved = await workspace.get('recent:sort');
    if (typeof saved === 'string' && SORTS.includes(saved)) {
      sort = saved;
      sortSelect.setValue(sort);
    }
    await load();
  }

  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  // A view (touch) never sets metarecords:dirty, so the snapshot is stable while
  // navigating; a real metarecord change (rename, delete, revert) does, and
  // refreshes the display fields.
  workspace.onChange('metarecords:dirty', () => {
    if (repo !== null) void load();
  });
  metafolder.whenVisible(deferredStart);
}
