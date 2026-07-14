// @ts-nocheck — not typed yet; the JS is being converted file by file.
// ref-list panel: lists the metarecords whose chosen Ref field points to the
// tree node currently selected in the treeref panel (via `selected_treeref`),
// either exactly or including the node's descendants in the tree forest. Picks
// a Ref field name from the repo's Ref fields. Selecting a row publishes
// `selected_metarecord` / `selected_paths`. Spec-gui "ref-list panel type".

import { el } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { refListQuery } from './queries.js';

const PAGE_DEFAULT = 100;

export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, cache } = metafolder;
  const REFRESH = cache.REFRESH;
  const PAGE = metafolder.pageSize ?? PAGE_DEFAULT;

  let repo = null; // repo used to list Ref fields (the active repo)
  let target = null; // {repo, field (tree field), uuid, path} from selected_treeref
  let refField = null;
  let mode = 'exact';
  let records = []; // metarecord objects (select '*') referencing the node
  let nextCursor = null;
  let cursorIndex = -1;
  let loading = false;
  let repoRoots = new Map(); // repo -> absolute root path (for selected_paths)

  const fieldSelect = root.getElementById('field');
  const modeSelect = root.getElementById('mode');
  const targetLine = root.getElementById('target');
  const entriesList = root.getElementById('entries');
  const placeholderElement = root.getElementById('placeholder');
  const statusLine = root.getElementById('status-line');
  const listingElement = root.getElementById('listing');

  // The repo the queries run against: the selected node's repo when there is a
  // selection, else the active repo (for populating the field picker).
  const queryRepo = () => target?.repo ?? repo;

  async function rootOf(r) {
    if (!repoRoots.has(r)) repoRoots.set(r, await daemon.repoRoot(r));
    return repoRoots.get(r);
  }

  // ── Field picker ──────────────────────────────────────────────────────────

  async function loadFields() {
    let list = [];
    const r = queryRepo();
    if (r) {
      try {
        list = (await daemon.call('GET', `/repos/${r}/fields?type=ref`)) ?? [];
      } catch (error) {
        await statusBar.error(error);
      }
    }
    const names = list.map((f) => f.name);
    if (refField === null && names.length > 0) refField = names[0];
    if (refField !== null && !names.includes(refField)) names.unshift(refField);
    fieldSelect.replaceChildren(
      ...names.map((name) => el('option', { value: name, selected: name === refField }, name)),
    );
    fieldSelect.disabled = names.length === 0;
  }

  // ── Fetch ─────────────────────────────────────────────────────────────────

  function renderTarget() {
    if (!target) {
      targetLine.textContent = 'No tree node selected.';
    } else {
      targetLine.textContent = `${target.field}: /${target.path}`;
    }
  }

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
      const fetched = result.uuids.map((u) => cache.readMetarecord(r, u)).filter((m) => m !== REFRESH);
      records = records.concat(fetched);
      nextCursor = result.nextCursor;
      // Resolve mfr_path for display / selected_paths (best-effort).
      await cache.fetchTreeRefs(r, 'mfr_path', fetched.map((m) => m.uuid));
      render();
    } finally {
      loading = false;
    }
  }

  // Relative mfr_path positions of a record (read from the cache), or [].
  function relPathsOf(record) {
    const paths = cache.readTreeRef(queryRepo(), 'mfr_path', record.uuid);
    return paths === REFRESH ? [] : paths;
  }

  function displayName(record) {
    const rel = relPathsOf(record)[0];
    if (rel === undefined) return record.uuid.slice(0, 8);
    return rel === '' ? '/' : (rel.split('/').pop() || rel);
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

  async function select(index) {
    cursorIndex = Math.max(0, Math.min(index, records.length - 1));
    render();
    const record = records[cursorIndex];
    if (!record) return;
    root.querySelector('li.cursor')?.scrollIntoView({ block: 'nearest' });
    const r = queryRepo();
    const rootPath = await rootOf(r);
    const paths = relPathsOf(record).map((rel) => (rel === '' ? rootPath : `${rootPath}/${rel}`));
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

  fieldSelect.addEventListener('change', () => {
    refField = fieldSelect.value;
    void fetchPage(true);
  });
  modeSelect.addEventListener('change', () => {
    mode = modeSelect.value === 'descendants' ? 'descendants' : 'exact';
    void fetchPage(true);
  });
  root.getElementById('refresh').addEventListener('click', () => void refresh());

  async function refresh() {
    await loadFields();
    await fetchPage(true);
  }

  commands.register('ref-list:next', {
    label: 'Ref list: move the cursor down',
    handler: () => select(cursorIndex + 1),
  });
  commands.register('ref-list:prev', {
    label: 'Ref list: move the cursor up',
    handler: () => select(cursorIndex - 1),
  });
  commands.register('ref-list:first', {
    label: 'Ref list: move to the first row',
    handler: () => select(0),
  });
  commands.register('ref-list:last', {
    label: 'Ref list: move to the last loaded row',
    handler: () => select(records.length - 1),
  });
  commands.register('ref-list:open', {
    label: 'Ref list: open the selection in the other panel',
    handler: openSelected,
  });
  commands.register('ref-list:toggle-scope', {
    label: 'Ref list: toggle between exact and + descendants',
    handler: () => {
      mode = mode === 'descendants' ? 'exact' : 'descendants';
      modeSelect.value = mode;
      void fetchPage(true);
    },
  });
  commands.register('ref-list:refresh', {
    label: 'Ref list: reload from the daemon',
    handler: () => refresh(),
  });

  // Keybindings for this panel live in keybindings.toml (when = "ref-list").

  async function start() {
    repo = await workspace.get('active_repo');
    target = (await workspace.get('selected_treeref')) ?? null;
    modeSelect.value = mode;
    await loadFields();
    await fetchPage(true);
  }

  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  workspace.onChange('selected_treeref', (value) => {
    target = value ?? null;
    metafolder.whenVisible(async () => {
      await loadFields(); // the node's repo may differ from the active one
      await fetchPage(true);
    });
  });
  workspace.onChange('metarecords:dirty', () => {
    if (!target) return;
    void fetchPage(true);
  });

  metafolder.whenVisible(deferredStart);

  return () => detachScroll();
}
