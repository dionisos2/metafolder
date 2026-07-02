// treeref panel: explore a TreeRef field's forest like a file explorer. Pick a
// TreeRef field name (e.g. mfr_path, or a tag tree), then descend from the
// roots to the leaves. Selecting a node publishes `selected_treeref` (consumed
// by the ref-list panel) and `selected_metarecord` (consumed by the detail /
// file panels). Spec-gui "treeref panel type".

import { el } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { childrenQuery, treeNameOf } from './queries.js';

const PAGE_DEFAULT = 200;
const DEFAULT_FIELD = 'mfr_path';

export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, cache } = metafolder;
  const REFRESH = cache.REFRESH;
  const PAGE = metafolder.pageSize ?? PAGE_DEFAULT;

  let repo = null;
  let field = DEFAULT_FIELD;
  let stack = []; // [{uuid, name}] — the path from a forest root to the current node
  let children = []; // metarecord objects (select '*') of the current node's direct children
  let nextCursor = null;
  let cursorIndex = -1;
  let loading = false;

  const fieldSelect = root.getElementById('field');
  const entriesList = root.getElementById('entries');
  const placeholderElement = root.getElementById('placeholder');
  const breadcrumb = root.getElementById('breadcrumb');
  const statusLine = root.getElementById('status-line');
  const listingElement = root.getElementById('listing');

  // Current node = the last breadcrumb entry; null UUID = the forest roots.
  const currentUuid = () => (stack.length > 0 ? stack[stack.length - 1].uuid : null);
  // Root-relative path of the current node ("" at the roots).
  const currentPath = () => stack.map((c) => c.name).join('/');

  // ── Field picker ──────────────────────────────────────────────────────────

  async function loadFields() {
    let list = [];
    try {
      list = (await daemon.call('GET', `/repos/${repo}/fields?type=tree_ref`)) ?? [];
    } catch (error) {
      await statusBar.error(error);
    }
    const names = list.map((f) => f.name);
    // The current field stays selectable even if the list is momentarily empty.
    if (!names.includes(field)) names.unshift(field);
    fieldSelect.replaceChildren(
      ...names.map((name) => el('option', { value: name, selected: name === field }, name)),
    );
  }

  // ── Navigation ──────────────────────────────────────────────────────────

  // `children` holds normalized {uuid, name} nodes (from the roots endpoint at
  // the top level, or from a Follows page below it).
  async function fetchChildren(reset) {
    if (!repo || loading) return;
    loading = true;
    try {
      if (reset) {
        await cache.sync(repo);
        children = [];
        nextCursor = null;
        cursorIndex = -1;
      }
      try {
        if (currentUuid() === null) {
          // Forest roots: their parent is the root sentinel, not reachable via
          // Follows — fetch them from the dedicated endpoint (unpaginated; a
          // forest has few roots). Only on a reset (no cursor at the top level).
          if (reset) {
            const roots =
              (await daemon.call('GET', `/repos/${repo}/tree/roots?field=${encodeURIComponent(field)}`)) ??
              [];
            children = roots.map((r) => ({ uuid: r.uuid, name: r.name }));
            nextCursor = null;
          }
        } else {
          const result = await cache.query(repo, {
            query: childrenQuery(field, currentUuid()),
            select: '*',
            limit: PAGE,
            ...(nextCursor && { cursor: nextCursor }),
          });
          const fetched = result.uuids
            .map((u) => cache.readMetarecord(repo, u))
            .filter((m) => m !== REFRESH)
            .map((m) => ({ uuid: m.uuid, name: treeNameOf(m, field) ?? '?' }));
          children = children.concat(fetched);
          nextCursor = result.nextCursor;
        }
      } catch (error) {
        await statusBar.error(error);
        return;
      }
      render();
    } finally {
      loading = false;
    }
  }

  function descend(index) {
    const child = children[index];
    if (!child) return;
    stack = [...stack, { uuid: child.uuid, name: child.name }];
    void fetchChildren(true);
  }

  function goUp() {
    if (stack.length === 0) return;
    stack = stack.slice(0, -1);
    void fetchChildren(true);
  }

  function gotoRoot() {
    if (stack.length === 0) return;
    stack = [];
    void fetchChildren(true);
  }

  // Jump to breadcrumb depth `depth` (0 = root, 1 = first crumb, …).
  function gotoDepth(depth) {
    if (depth >= stack.length) return;
    stack = stack.slice(0, depth);
    void fetchChildren(true);
  }

  async function select(index) {
    cursorIndex = Math.max(0, Math.min(index, children.length - 1));
    render();
    const child = children[cursorIndex];
    if (!child) return;
    root.querySelector('li.cursor')?.scrollIntoView({ block: 'nearest' });
    const path = [...stack.map((c) => c.name), child.name].filter((s) => s !== '').join('/');
    await workspace.set('selected_metarecord', { uuid: child.uuid, repo });
    await workspace.set('selected_treeref', { repo, field, uuid: child.uuid, path });
  }

  // ── Rendering ─────────────────────────────────────────────────────────────

  // Display label of a node: the root metarecord's empty name shows as "/";
  // an otherwise-empty name falls back to a short uuid.
  const nodeLabel = (node) => (node.name === '' ? '/' : node.name || node.uuid.slice(0, 8));

  function render() {
    placeholderElement.hidden = children.length > 0 || loading;
    placeholderElement.textContent = loading
      ? 'Loading…'
      : stack.length === 0
        ? 'No roots in this forest.'
        : 'No children (leaf node).';

    breadcrumb.replaceChildren(
      el('span', { class: 'crumb', onclick: () => gotoRoot() }, `${field}:/`),
      ...stack.flatMap((crumb, depth) => [
        el('span', {}, depth === 0 ? '' : '/'),
        el('span', { class: 'crumb', onclick: () => gotoDepth(depth + 1) }, nodeLabel(crumb)),
      ]),
    );

    entriesList.replaceChildren(
      ...children.map((child, index) =>
        el(
          'li',
          {
            class: [index === cursorIndex && 'cursor'],
            onclick: () => select(index),
            ondblclick: () => descend(index),
          },
          el('span', { class: 'icon' }, '🏷️'),
          el('span', { class: 'name' }, nodeLabel(child)),
        ),
      ),
    );

    statusLine.textContent =
      `${children.length}${nextCursor ? '+' : ''} ` +
      `child${children.length === 1 ? '' : 'ren'}` +
      (nextCursor ? ' (more — scroll down)' : '');
  }

  // ── Wiring ──────────────────────────────────────────────────────────────

  const pager = createPagedList({
    loaded: () => children.length,
    total: () => null,
    hasMore: () => nextCursor !== null,
    loadMore: () => fetchChildren(false),
  });
  const detachScroll = pager.attach(listingElement);

  fieldSelect.addEventListener('change', () => {
    field = fieldSelect.value;
    stack = [];
    void fetchChildren(true);
  });
  root.getElementById('root').addEventListener('click', gotoRoot);
  root.getElementById('up').addEventListener('click', goUp);
  root.getElementById('refresh').addEventListener('click', () => void refresh());

  async function refresh() {
    await loadFields();
    await fetchChildren(true);
  }

  commands.register('treeref:next', {
    label: 'TreeRef explorer: move the cursor down',
    handler: () => select(cursorIndex + 1),
  });
  commands.register('treeref:prev', {
    label: 'TreeRef explorer: move the cursor up',
    handler: () => select(cursorIndex - 1),
  });
  commands.register('treeref:first', {
    label: 'TreeRef explorer: move to the first child',
    handler: () => select(0),
  });
  commands.register('treeref:last', {
    label: 'TreeRef explorer: move to the last loaded child',
    handler: () => select(children.length - 1),
  });
  commands.register('treeref:descend', {
    label: 'TreeRef explorer: descend into the selected node',
    handler: () => descend(cursorIndex),
  });
  commands.register('treeref:parent', {
    label: 'TreeRef explorer: go up one level',
    handler: goUp,
  });
  commands.register('treeref:root', {
    label: 'TreeRef explorer: jump to the forest roots',
    handler: gotoRoot,
  });
  commands.register('treeref:refresh', {
    label: 'TreeRef explorer: reload from the daemon',
    handler: () => refresh(),
  });

  // Keybindings for this panel live in keybindings.toml (when = "treeref").

  async function start() {
    repo = await workspace.get('active_repo');
    if (repo === null) {
      placeholderElement.hidden = false;
      placeholderElement.textContent = 'No active repository.';
      fieldSelect.disabled = true;
      return;
    }
    fieldSelect.disabled = false;
    // A value picker (spec-gui "Value picker") can seed the field to explore.
    const seedField = await workspace.get('treeref:field');
    if (typeof seedField === 'string' && seedField) field = seedField;
    await loadFields();
    stack = [];
    await fetchChildren(true);
  }

  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  workspace.onChange('metarecords:dirty', () => {
    if (repo === null) return;
    void fetchChildren(true);
  });

  metafolder.whenVisible(deferredStart);

  return () => detachScroll();
}
