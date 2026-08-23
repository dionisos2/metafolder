// ref-list panel: lists the metarecords whose chosen Ref field points to the
// tree node currently selected in the treeref panel (via `selected_treeref`),
// either exactly or including the node's descendants in the tree forest. Picks
// a Ref field name from the repo's Ref fields. Selecting a row publishes
// `selected_metarecord` / `selected_paths`. Spec-gui "ref-list panel type".

import { byId, el, field } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { createSelect } from '/__select.js';
import { rowActionsProvider, baseName } from '/__file-actions.js';
import { refListQuery } from './queries.js';

const PAGE_DEFAULT = 100;

/**
 * The tree node the treeref panel published in `selected_treeref`.
 * @typedef {{repo: string, field: string, uuid: string, path: string}} Target
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, cache } = metafolder;
  // Annotated: an unannotated `const x = cache.REFRESH` widens the unique symbol
  // to plain `symbol`, and `value === x` then narrows nothing.
  /** @type {Metafolder.Refresh} */
  const REFRESH = cache.REFRESH;
  const PAGE = metafolder.pageSize ?? PAGE_DEFAULT;

  /** @type {string|null} repo used to list Ref fields (the active repo) */
  let repo = null;
  /** @type {Target|null} the node selected in the treeref panel */
  let target = null;
  /** @type {string|null} */
  let refField = null;
  let mode = 'exact';
  /** @type {Metafolder.Metarecord[]} metarecords (select '*') referencing the node */
  let records = [];
  /** @type {string|null} */
  let nextCursor = null;
  let cursorIndex = -1;
  let loading = false;
  /** @type {Map<string, string>} repo -> absolute root path (for selected_paths) */
  const repoRoots = new Map();

  // `fetchPage` is a hoisted function declaration, so onChange can name it here.
  const fieldSelect = createSelect(byId(root, 'field'), {
    placeholder: 'no ref field',
    onChange: (v) => {
      refField = v;
      void fetchPage(true);
    },
  });
  const modeSelect = createSelect(byId(root, 'mode'), {
    value: 'exact',
    options: [
      { value: 'exact', label: 'exact' },
      { value: 'descendants', label: '+ descendants' },
    ],
    onChange: (v) => {
      mode = v === 'descendants' ? 'descendants' : 'exact';
      void fetchPage(true);
    },
  });
  const targetLine = byId(root, 'target');
  const entriesList = byId(root, 'entries');
  const placeholderElement = byId(root, 'placeholder');
  const statusLine = byId(root, 'status-line');
  const listingElement = byId(root, 'listing');

  // The repo the queries run against: the selected node's repo when there is a
  // selection, else the active repo (for populating the field picker).
  const queryRepo = () => target?.repo ?? repo;

  /** @param {string} r @returns {Promise<string>} */
  async function rootOf(r) {
    const known = repoRoots.get(r);
    if (known !== undefined) return known;
    const path = await daemon.repoRoot(r);
    repoRoots.set(r, path);
    return path;
  }

  // ── Field picker ──────────────────────────────────────────────────────────

  async function loadFields() {
    /** @type {{name: string}[]} */
    let list = [];
    const r = queryRepo();
    if (r) {
      try {
        list = /** @type {{name: string}[]} */ (
          (await daemon.call('GET', `/repos/${r}/fields?type=ref`)) ?? []
        );
      } catch (error) {
        await statusBar.error(error);
      }
    }
    const names = list.map((f) => f.name);
    if (refField === null && names.length > 0) refField = names[0];
    if (refField !== null && !names.includes(refField)) names.unshift(refField);
    fieldSelect.setOptions(
      names.map((name) => ({ value: name })),
      refField,
    );
    fieldSelect.element.toggleAttribute('disabled', names.length === 0);
  }

  // ── Fetch ─────────────────────────────────────────────────────────────────

  function renderTarget() {
    if (!target) {
      targetLine.textContent = 'No tree node selected.';
    } else {
      targetLine.textContent = `${target.field}: /${target.path}`;
    }
  }

  /** @param {boolean} reset */
  async function fetchPage(reset) {
    const r = queryRepo();
    if (!r || !target || !refField || loading) {
      if (reset) {
        records = [];
        nextCursor = null;
        cursorIndex = -1;
        render();
      }
      return;
    }
    loading = true;
    try {
      if (reset) {
        await cache.sync(r);
        records = [];
        nextCursor = null;
        cursorIndex = -1;
      }
      let result;
      try {
        result = await cache.query(r, {
          query: refListQuery({ refField, treeField: target.field, uuid: target.uuid, mode }),
          select: '*',
          limit: PAGE,
          ...(nextCursor && { cursor: nextCursor }),
        });
      } catch (error) {
        await statusBar.error(error);
        return;
      }
      const fetched = /** @type {Metafolder.Metarecord[]} */ (
        result.uuids.map((u) => cache.readMetarecord(r, u)).filter((m) => m !== REFRESH)
      );
      records = records.concat(fetched);
      nextCursor = result.nextCursor;
      // Resolve mfr_path for display / selected_paths (best-effort).
      await cache.fetchTreeRefs(r, 'mfr_path', fetched.map((m) => m.uuid));
      // Warm the repo root so the right-click file menu can build absolute paths
      // synchronously in render().
      await rootOf(r).catch(() => {});
      render();
    } finally {
      loading = false;
    }
  }

  // Relative mfr_path positions of a record (read from the cache), or [].
  /** @param {Metafolder.Metarecord} record @returns {string[]} */
  function relPathsOf(record) {
    const r = queryRepo();
    if (!r) return [];
    const paths = cache.readTreeRef(r, 'mfr_path', record.uuid);
    if (paths === REFRESH) return [];
    return paths;
  }

  /** @param {Metafolder.Metarecord} record */
  function displayName(record) {
    const rel = relPathsOf(record)[0];
    if (rel === undefined) return record.uuid.slice(0, 8);
    return rel === '' ? '/' : (rel.split('/').pop() || rel);
  }

  // `data-mf-*` attributes for the shared right-click file menu
  // (/__file-actions.js). Empty until the repo root is cached or when the record
  // has no on-disk path.
  /** @param {Metafolder.Metarecord} record */
  function fileRowAttrs(record) {
    const r = queryRepo();
    const rootPath = r ? repoRoots.get(r) : undefined;
    const rel = relPathsOf(record)[0];
    if (!r || rootPath === undefined || rel === undefined) return {};
    // `rel` is leading-"/"-rooted ('' is the repo root), so concatenate directly.
    const abs = rel === '' ? rootPath : `${rootPath}${rel}`;
    const typeValue = field(record, 'mfr_type')?.value;
    const isDir = typeValue?.type === 'string' && typeValue.value === 'dir';
    return { 'data-mf-path': abs, 'data-mf-isdir': isDir ? '1' : '0', 'data-mf-name': baseName(abs) };
  }

  // ── Rendering ─────────────────────────────────────────────────────────────

  function render() {
    renderTarget();
    placeholderElement.hidden = records.length > 0;
    placeholderElement.textContent = !target
      ? 'Select a node in the tree panel.'
      : !refField
        ? 'This repository has no Ref field.'
        : 'No metarecord references this node.';

    entriesList.replaceChildren(
      ...records.map((record, index) =>
        el(
          'li',
          {
            class: [index === cursorIndex && 'cursor'],
            onclick: () => select(index),
            ondblclick: () => openSelected(),
            'data-mf-uuid': record.uuid,
            ...fileRowAttrs(record),
          },
          el('span', { class: 'name' }, displayName(record)),
        ),
      ),
    );

    statusLine.textContent =
      `${records.length}${nextCursor ? '+' : ''} ` +
      `metarecord${records.length === 1 ? '' : 's'}` +
      (nextCursor ? ' (more — scroll down)' : '');
  }

  /** @param {number} index */
  async function select(index) {
    cursorIndex = Math.max(0, Math.min(index, records.length - 1));
    render();
    const record = records[cursorIndex];
    if (!record) return;
    root.querySelector('li.cursor')?.scrollIntoView({ block: 'nearest' });
    const r = queryRepo();
    if (!r) return;
    const rootPath = await rootOf(r);
    const paths = relPathsOf(record).map((rel) => (rel === '' ? rootPath : `${rootPath}${rel}`));
    await workspace.set('selected_metarecord', { uuid: record.uuid, repo: r });
    await workspace.set('selected_paths', paths);
  }

  async function openSelected() {
    const record = records[cursorIndex];
    if (!record) return;
    const paths = relPathsOf(record);
    await commands.invoke(`panel:reveal-other ${paths.length > 0 ? 'file' : 'metarecord-detail'}`);
  }

  // ── Wiring ──────────────────────────────────────────────────────────────

  const pager = createPagedList({
    loaded: () => records.length,
    total: () => null,
    hasMore: () => nextCursor !== null,
    loadMore: () => fetchPage(false),
  });
  const detachScroll = pager.attach(listingElement);

  byId(root, 'refresh').addEventListener('click', () => void refresh());

  async function refresh() {
    await loadFields();
    await fetchPage(true);
  }

  void commands.register('ref-list:next', {
    label: 'Ref list: move the cursor down',
    handler: () => select(cursorIndex + 1),
  });
  void commands.register('ref-list:prev', {
    label: 'Ref list: move the cursor up',
    handler: () => select(cursorIndex - 1),
  });
  void commands.register('ref-list:first', {
    label: 'Ref list: move to the first row',
    handler: () => select(0),
  });
  void commands.register('ref-list:last', {
    label: 'Ref list: move to the last loaded row',
    handler: () => select(records.length - 1),
  });
  void commands.register('ref-list:open', {
    label: 'Ref list: open the selection in the other panel',
    handler: openSelected,
  });
  void commands.register('ref-list:toggle-scope', {
    label: 'Ref list: toggle between exact and + descendants',
    handler: () => {
      mode = mode === 'descendants' ? 'exact' : 'descendants';
      modeSelect.setValue(mode);
      void fetchPage(true);
    },
  });
  void commands.register('ref-list:refresh', {
    label: 'Ref list: reload from the daemon',
    handler: () => refresh(),
  });

  // Keybindings for this panel live in keybindings.toml (when = "ref-list").

  // Right-click a row: the shared "Metarecord" section (open in detail/file,
  // reveal folder, Copy UUID) plus, when the record is file-backed, the file
  // actions (cut/copy/paste/rename/duplicate/trash) — see /__file-actions.js.
  metafolder.contextMenu.addDefaultItems(rowActionsProvider(metafolder, () => queryRepo()));

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    target = /** @type {Target|null} */ ((await workspace.get('selected_treeref')) ?? null);
    modeSelect.setValue(mode);
    await loadFields();
    await fetchPage(true);
  }

  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  async function reloadForTarget() {
    await loadFields(); // the node's repo may differ from the active one
    await fetchPage(true);
  }
  workspace.onChange('selected_treeref', (value) => {
    target = /** @type {Target|null} */ (value ?? null);
    metafolder.whenVisible(() => void reloadForTarget());
  });
  workspace.onChange('metarecords:dirty', () => {
    if (!target) return;
    void fetchPage(true);
  });

  metafolder.whenVisible(deferredStart);

  return () => detachScroll();
}
