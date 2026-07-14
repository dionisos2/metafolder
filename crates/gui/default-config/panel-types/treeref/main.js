// treeref panel: explore a TreeRef field's forest like a file explorer. Pick a
// TreeRef field name (e.g. mfr_path, or a tag tree), then descend from the
// roots to the leaves. Selecting a node publishes `selected_treeref` (consumed
// by the ref-list panel) and `selected_metarecord` (consumed by the detail /
// file panels). Spec-gui "treeref panel type".

import { byId, el } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { childrenQuery, treeNameOf } from './queries.js';

const PAGE_DEFAULT = 200;
const DEFAULT_FIELD = 'mfr_path';

/**
 * A node of the forest, as this panel handles it: the roots endpoint and a
 * Follows page are normalized to the same shape.
 * @typedef {{uuid: string, name: string}} Node
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

  /** @type {string|null} */
  let repo = null;
  let field = DEFAULT_FIELD;
  /** @type {Node[]} the path from a forest root to the current node */
  let stack = [];
  /** @type {Node[]} the current node's direct children */
  let children = [];
  /** @type {string|null} */
  let nextCursor = null;
  let cursorIndex = -1;
  let loading = false;

  const fieldSelect = byId(root, 'field', HTMLSelectElement);
  const entriesList = byId(root, 'entries');
  const placeholderElement = byId(root, 'placeholder');
  const breadcrumb = byId(root, 'breadcrumb');
  const statusLine = byId(root, 'status-line');
  const listingElement = byId(root, 'listing');

  // Current node = the last breadcrumb entry; null UUID = the forest roots.
  const currentUuid = () => (stack.length > 0 ? stack[stack.length - 1].uuid : null);

  // ── Field picker ──────────────────────────────────────────────────────────

  async function loadFields() {
    /** @type {{name: string}[]} */
    let list = [];
    try {
      list = /** @type {{name: string}[]} */ (
        (await daemon.call('GET', `/repos/${repo}/fields?type=tree_ref`)) ?? []
      );
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
  /** @param {boolean} reset */
  async function fetchChildren(reset) {
    if (!repo || loading) return;
    // Held in a const: `repo` is a captured `let`, so the guard above does not
    // narrow it inside the callbacks below.
    const r = repo;
    loading = true;
    try {
      if (reset) {
        await cache.sync(r);
        children = [];
        nextCursor = null;
        cursorIndex = -1;
      }
      try {
        const current = currentUuid();
        if (current === null) {
          // Forest roots: their parent is the root sentinel, not reachable via
          // Follows — fetch them from the dedicated endpoint (unpaginated; a
          // forest has few roots). Only on a reset (no cursor at the top level).
          if (reset) {
            const roots = /** @type {Node[]} */ (
              (await daemon.call(
                'GET',
                `/repos/${r}/tree/roots?field=${encodeURIComponent(field)}`,
              )) ?? []
            );
            children = roots.map((r) => ({ uuid: r.uuid, name: r.name }));
            nextCursor = null;
          }
        } else {
          const result = await cache.query(r, {
            query: childrenQuery(field, current),
            select: '*',
            limit: PAGE,
            ...(nextCursor && { cursor: nextCursor }),
          });
          const fetched = /** @type {Metafolder.Metarecord[]} */ (
            result.uuids.map((u) => cache.readMetarecord(r, u)).filter((m) => m !== REFRESH)
          ).map((m) => ({ uuid: m.uuid, name: treeNameOf(m, field) ?? '?' }));
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

  /** @param {number} index */
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
  /** @param {number} depth */
  function gotoDepth(depth) {
    if (depth >= stack.length) return;
    stack = stack.slice(0, depth);
    void fetchChildren(true);
  }

  /** @param {number} index */
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
  /** @param {Node} node */
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
  byId(root, 'root').addEventListener('click', gotoRoot);
  byId(root, 'up').addEventListener('click', goUp);
  byId(root, 'refresh').addEventListener('click', () => void refresh());

  async function refresh() {
    await loadFields();
    await fetchChildren(true);
  }

  void commands.register('treeref:next', {
    label: 'TreeRef explorer: move the cursor down',
    handler: () => select(cursorIndex + 1),
  });
  void commands.register('treeref:prev', {
    label: 'TreeRef explorer: move the cursor up',
    handler: () => select(cursorIndex - 1),
  });
  void commands.register('treeref:first', {
    label: 'TreeRef explorer: move to the first child',
    handler: () => select(0),
  });
  void commands.register('treeref:last', {
    label: 'TreeRef explorer: move to the last loaded child',
    handler: () => select(children.length - 1),
  });
  void commands.register('treeref:descend', {
    label: 'TreeRef explorer: descend into the selected node',
    handler: () => descend(cursorIndex),
  });
  void commands.register('treeref:parent', {
    label: 'TreeRef explorer: go up one level',
    handler: goUp,
  });
  void commands.register('treeref:root', {
    label: 'TreeRef explorer: jump to the forest roots',
    handler: gotoRoot,
  });
  void commands.register('treeref:refresh', {
    label: 'TreeRef explorer: reload from the daemon',
    handler: () => refresh(),
  });

  // Keybindings for this panel live in keybindings.toml (when = "treeref").

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
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
