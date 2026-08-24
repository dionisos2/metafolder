// treeref panel: explore a TreeRef field's forest like a file explorer. Pick a
// TreeRef field name (e.g. mfr_path, or a tag tree), then descend from the
// roots to the leaves. Selecting a node publishes `selected_treeref` (consumed
// by the ref-list panel) and `selected_metarecord` (consumed by the detail /
// file panels). Spec-gui "treeref panel type".

import { byId, el } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { createSelect } from '/__select.js';
import { fileActionsProvider, metarecordMenuItems } from '/__file-actions.js';
import { childrenQuery, treeNameOf, treeRefPath } from './queries.js';

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
  let picking = false; // true while this panel is open as a tree_ref value picker
  // The active repo's root path, cached for building absolute file paths for the
  // right-click file menu (only meaningful for the mfr_path forest).
  /** @type {string|null} */
  let repoRootPath = null;
  /** @type {string|null} the repo repoRootPath was fetched for */
  let repoRootFor = null;

  // `fetchChildren` is a hoisted function declaration, so onChange can name it.
  const fieldSelect = createSelect(byId(root, 'field'), {
    value: field,
    options: [{ value: field }],
    onChange: (v) => {
      field = v;
      stack = [];
      void fetchChildren(true);
    },
  });
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
    fieldSelect.setOptions(
      names.map((name) => ({ value: name })),
      field,
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
    // Warm the repo root once per repo so render() can build absolute paths.
    if (repoRootFor !== r) {
      repoRootFor = r;
      repoRootPath = await daemon.repoRoot(r).catch(() => null);
    }
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
          const fetched = /** @type {Metafolder.Metarecord[]} */ (result.records).map((m) => ({
            uuid: m.uuid,
            name: treeNameOf(m, field) ?? '?',
          }));
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
    // Publish the path in the daemon's convention (see treeRefPath).
    const path = treeRefPath([...stack.map((c) => c.name), child.name]);
    await workspace.set('selected_metarecord', { uuid: child.uuid, repo });
    await workspace.set('selected_treeref', { repo, field, uuid: child.uuid, path });
  }

  // ── Rendering ─────────────────────────────────────────────────────────────

  // Display label of a node: the root metarecord's empty name shows as "/";
  // an otherwise-empty name falls back to a short uuid.
  /** @param {Node} node */
  const nodeLabel = (node) => (node.name === '' ? '/' : node.name || node.uuid.slice(0, 8));

  // `data-mf-*` attributes for the shared right-click file menu — only for the
  // mfr_path forest, whose nodes map to on-disk paths. isDir is left unset (a
  // node may be a directory or a leaf file), so the menu probes when pasting.
  /** @param {Node} child @returns {Record<string, string>} */
  function fileRowAttrs(child) {
    if (field !== 'mfr_path' || repoRootPath === null) return {};
    const rel = [...stack.map((c) => c.name), child.name].filter((s) => s !== '').join('/');
    const abs = rel === '' ? repoRootPath : `${repoRootPath}/${rel}`;
    return { 'data-mf-path': abs, 'data-mf-name': child.name || rel };
  }

  function render() {
    placeholderElement.hidden = children.length > 0 || loading;
    placeholderElement.textContent = loading
      ? 'Loading…'
      : stack.length === 0
        ? 'No roots in this forest.'
        : 'No children (leaf node).';

    breadcrumb.replaceChildren(
      el('span', { class: 'crumb', onclick: () => gotoRoot() }, `${field}:`),
      ...stack.flatMap((crumb, depth) => {
        // Separator before this crumb: none for the first node, and none right
        // after the filesystem root (its label is already "/"), so we never
        // double the slash ("mfr_path:///projets"). Otherwise a single "/".
        const sep = depth === 0 || stack[depth - 1].name === '' ? '' : '/';
        return [
          el('span', {}, sep),
          el('span', { class: 'crumb', onclick: () => gotoDepth(depth + 1) }, nodeLabel(crumb)),
        ];
      }),
    );

    entriesList.replaceChildren(
      ...children.map((child, index) =>
        el(
          'li',
          {
            class: [index === cursorIndex && 'cursor'],
            onclick: () => select(index),
            ondblclick: () => descend(index),
            ...fileRowAttrs(child),
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

  /** The index in `entriesList` of the node `li` under a context-menu event, or
   *  -1 when the click missed a row. The event is handled at the shell `window`,
   *  so the real clicked node is found through `composedPath()`.
   *  @param {MouseEvent} event */
  function liIndexFromEvent(event) {
    for (const node of event.composedPath()) {
      if (node instanceof Element && node.matches('li')) {
        const index = [...entriesList.children].indexOf(node);
        if (index >= 0) return index;
      }
    }
    return -1;
  }

  // Right-click a node in the mfr_path forest to cut/copy/paste/rename/duplicate
  // /trash the file or directory it maps to (shared with the file manager). When
  // this panel is open as a tree_ref value picker, the node also gets a "Pick
  // this folder" (mfr_path) / "Pick this node" (any other TreeRef) item that
  // confirms the pick (its uuid becomes the TreeRef parent) — the same
  // affordance the metarecord-list picker offers.
  const fileActions = fileActionsProvider(metafolder, () => repo);
  metafolder.contextMenu.addDefaultItems((event) => {
    const index = liIndexFromEvent(event);
    // Make the clicked node the selection so `pick:confirm` and "Open in panel
    // metarecord-detail" read its uuid, not whatever the cursor last sat on.
    if (index >= 0 && index !== cursorIndex) void select(index);
    const node = children[index >= 0 ? index : cursorIndex];
    /** @type {Metafolder.MenuItem[]} */
    const items = [];
    if (node) {
      // A picker also offers "Pick this folder" (mfr_path) / "Pick this node"
      // (any other TreeRef) — worded per forest, as only mfr_path maps to disk.
      const pickLabel = field === 'mfr_path' ? 'Pick this folder' : 'Pick this node';
      // No "Open in panel file"/"reveal folder" here: this panel does not
      // publish `selected_paths`, which those commands need. The file actions
      // (cut/copy/…) still come from `fileActions` via the row's data-mf-path.
      items.push(
        ...metarecordMenuItems({
          metafolder,
          uuid: node.uuid,
          hasFile: false,
          leading: picking
            ? [{ label: pickLabel, action: () => void commands.invoke('pick:confirm') }]
            : [],
        }),
      );
    }
    items.push(...fileActions(event));
    return items;
  });

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    if (repo === null) {
      placeholderElement.hidden = false;
      placeholderElement.textContent = 'No active repository.';
      fieldSelect.element.toggleAttribute('disabled', true);
      return;
    }
    fieldSelect.element.toggleAttribute('disabled', false);
    // A value picker (spec-gui "Value picker") can seed the field to explore and
    // arms the "Pick this folder/node" context-menu item.
    picking = !!(await workspace.get('pick_request'));
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
