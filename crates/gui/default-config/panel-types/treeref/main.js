// treeref panel: explore a TreeRef field's forest like a file explorer. Pick a
// TreeRef field name (e.g. mfr_path, or a tag tree), then descend from the
// roots to the leaves. Selecting a node publishes `selected_treeref` and
// `selected_metarecord` (consumed by the detail / file panels).
//
// It also answers "what points at this node?" — `treeref:list-refs` builds the
// query `<ref field> -> (<tree field> = "<path>")` and opens it in
// metarecord-list, the ref bar showing which query the command will run. That
// replaces the former `ref-list` panel: the answer is an ordinary list with a
// visible, editable query rather than a second panel type with its own display.
// Spec-gui "treeref panel type".

import { byId, el } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { createSelect } from '/__select.js';
import { fileActionsProvider, metarecordMenuItems } from '/__file-actions.js';
import { registerFind } from '/__find-entry.js';
import { childrenQuery, refQueryDsl, treeNameOf, treeRefPath } from './queries.js';

const PAGE_DEFAULT = 200;
// The tree_ref field the panel opens on. The effective value comes from the
// GUI config (`[panel-defaults.treeref].field`), overridden by the stored
// `treeref:field` workspace variable; this is only the fallback.
const DEFAULT_FIELD = 'mfr_path';
// The Ref field `treeref:list-refs` follows back into the forest. Tags are by
// far the common case, so that is the default; `[panel-defaults.treeref]
// .ref-field` and the stored `treeref:ref-field` variable override it.
const DEFAULT_REF_FIELD = 'tag';

/**
 * A node of the forest, as this panel handles it: the roots endpoint and a
 * Follows page are normalized to the same shape.
 * @typedef {{uuid: string, name: string}} Node
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, cache } = metafolder;
  const PAGE = metafolder.pageSize ?? PAGE_DEFAULT;
  const defaultField = metafolder.defaults.field ?? DEFAULT_FIELD;
  const defaultRefField = metafolder.defaults.refField ?? DEFAULT_REF_FIELD;

  /** @type {string|null} */
  let repo = null;
  let field = defaultField;
  /** @type {Node[]} the path from a forest root to the current node */
  let stack = [];
  /** @type {Node[]} the current node's direct children */
  let children = [];
  /** @type {string|null} */
  let nextCursor = null;
  let cursorIndex = -1;
  let loading = false;
  let picking = false; // true while this panel is open as a tree_ref value picker
  /** @type {string} the Ref field `treeref:list-refs` follows */
  let refField = defaultRefField;
  /** @type {'exact'|'subtree'} how much of the selected node the query covers */
  let scope = 'exact';
  // The active repo's root path, cached for building absolute file paths for the
  // right-click file menu (only meaningful for the mfr_path forest).
  /** @type {string|null} */
  let repoRootPath = null;
  /** @type {string|null} the repo repoRootPath was fetched for */
  let repoRootFor = null;

  // `setField` is a hoisted function declaration, so onChange can name it.
  const fieldSelect = createSelect(byId(root, 'field'), {
    value: field,
    options: [{ value: field }],
    onChange: (v) => void setField(v),
  });
  const refFieldSelect = createSelect(byId(root, 'ref-field'), {
    value: refField,
    options: [{ value: refField }],
    onChange: (v) => void setRefField(v),
  });
  const scopeSelect = createSelect(byId(root, 'scope'), {
    value: scope,
    options: [
      { value: 'exact', label: 'exact' },
      { value: 'subtree', label: '+ descendants' },
    ],
    onChange: (v) => {
      scope = v === 'subtree' ? 'subtree' : 'exact';
      void workspace.set('treeref:scope', scope);
      renderQueryPreview();
    },
  });
  const queryPreview = byId(root, 'query-preview');
  const entriesList = byId(root, 'entries');
  const placeholderElement = byId(root, 'placeholder');
  const breadcrumb = byId(root, 'breadcrumb');
  const statusLine = byId(root, 'status-line');
  const listingElement = byId(root, 'listing');

  // Current node = the last breadcrumb entry; null UUID = the forest roots.
  const currentUuid = () => (stack.length > 0 ? stack[stack.length - 1].uuid : null);

  // ── Field picker ──────────────────────────────────────────────────────────

  /** The repo's TreeRef field names, the current one always among them (it
   *  stays selectable even if the catalog is momentarily empty). */
  async function treeRefFieldNames() {
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
    if (!names.includes(field)) names.unshift(field);
    return names;
  }

  /** Switches the explored forest to `name` and reloads from its roots. Shared
   *  by the drop-down's onChange and the `treeref:set-field` command.
   *  @param {string} name */
  async function setField(name) {
    if (name === field) return;
    field = name;
    stack = [];
    fieldSelect.setValue(name); // mirror the command into the drop-down
    renderQueryPreview();
    await fetchChildren(true);
  }

  async function loadFields() {
    const names = await treeRefFieldNames();
    fieldSelect.setOptions(
      names.map((name) => ({ value: name })),
      field,
    );
  }


  // ── "What points here?" ───────────────────────────────────────────────────

  /** The repo's Ref field names, the current one always among them. */
  async function refFieldNames() {
    /** @type {{name: string}[]} */
    let list = [];
    try {
      list = /** @type {{name: string}[]} */ (
        (await daemon.call('GET', `/repos/${repo}/fields?type=ref`)) ?? []
      );
    } catch (error) {
      await statusBar.error(error);
    }
    const names = list.map((f) => f.name);
    if (!names.includes(refField)) names.unshift(refField);
    return names;
  }

  /** @param {string} name */
  async function setRefField(name) {
    if (name === refField) return;
    refField = name;
    refFieldSelect.setValue(name);
    await workspace.set('treeref:ref-field', name);
    renderQueryPreview();
  }

  async function loadRefFields() {
    const names = await refFieldNames();
    refFieldSelect.setOptions(
      names.map((name) => ({ value: name })),
      refField,
    );
  }

  /** The path of the node under the cursor, or null when nothing is selected. */
  function selectedPath() {
    const child = children[cursorIndex];
    if (!child) return null;
    return treeRefPath([...stack.map((c) => c.name), child.name]);
  }

  /** The DSL `treeref:list-refs` would run right now — shown so the ref bar
   *  says what the command produces, not merely which field it uses. */
  function currentQueryDsl() {
    return refQueryDsl({ refField, treeField: field, path: selectedPath(), scope });
  }

  function renderQueryPreview() {
    queryPreview.textContent = currentQueryDsl();
  }

  /** Runs the query in metarecord-list, in the *other* slot: the tree stays
   *  visible so the next node can be asked about straight away — the pairing
   *  the ref-list panel used to provide. */
  async function listRefs() {
    if (selectedPath() === null) {
      await statusBar.message('select a node first');
      return;
    }
    await workspace.set('metarecord-list:query-request', {
      dsl: currentQueryDsl(),
      nonce: Date.now(),
    });
    await commands.invoke('panel:reveal-other metarecord-list');
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
    renderQueryPreview();
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
    renderQueryPreview();
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
    await loadRefFields();
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
  // The drop-down's keyboard equivalent: pick the TreeRef field to explore
  // without leaving the keyboard (spec-gui "treeref panel type").
  void commands.register('treeref:set-field', {
    label: 'TreeRef explorer: explore another TreeRef field',
    args: [
      {
        name: 'field',
        prompt: () => 'TreeRef field to explore?',
        // Deliberately NOT pre-filled with the current field: the answer names
        // another forest, so a pre-fill would only have to be erased first (the
        // drop-down already shows which field is current).
        complete: () => treeRefFieldNames(),
      },
    ],
    handler: (name) => setField(name.trim()),
  });

  // Jump to a child by name — the shared list-panel find, on the same key as
  // everywhere else. Only the *loaded* children are searched, as in a paged
  // list; the search is over the displayed label, so the mfr_path root is "/".
  void registerFind(metafolder, 'treeref:find', {
    label: 'TreeRef explorer: jump to a child by name',
    prompt: 'Go to child:',
    entries: () => children.map((child) => ({ name: nodeLabel(child) })),
    select,
  });

  void commands.register('treeref:list-refs', {
    label: 'TreeRef explorer: list the metarecords pointing at the selected node',
    handler: listRefs,
  });
  void commands.register('treeref:toggle-scope', {
    label: 'TreeRef explorer: toggle between the exact node and its whole subtree',
    handler: () => {
      scope = scope === 'subtree' ? 'exact' : 'subtree';
      scopeSelect.setValue(scope);
      void workspace.set('treeref:scope', scope);
      renderQueryPreview();
    },
  });
  void commands.register('treeref:set-ref-field', {
    label: 'TreeRef explorer: choose the Ref field to follow back',
    args: [
      {
        name: 'field',
        prompt: () => 'Ref field to follow back into the tree?',
        initial: () => refField,
        complete: () => refFieldNames(),
      },
    ],
    handler: (name) => setRefField(name.trim()),
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
      const listLabel =
        scope === 'subtree'
          ? `List what ${refField} points at here or below`
          : `List what ${refField} points at here`;
      items.push(
        ...metarecordMenuItems({
          metafolder,
          uuid: node.uuid,
          hasFile: false,
          leading: [
            ...(picking
              ? [{ label: pickLabel, action: () => void commands.invoke('pick:confirm') }]
              : []),
            { label: listLabel, action: () => void listRefs() },
          ],
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
      renderQueryPreview();
      return;
    }
    fieldSelect.element.toggleAttribute('disabled', false);
    // A value picker (spec-gui "Value picker") can seed the field to explore and
    // arms the "Pick this folder/node" context-menu item.
    picking = !!(await workspace.get('pick_request'));
    const seedField = await workspace.get('treeref:field');
    if (typeof seedField === 'string' && seedField) field = seedField;
    const seedRefField = await workspace.get('treeref:ref-field');
    if (typeof seedRefField === 'string' && seedRefField) refField = seedRefField;
    const seedScope = await workspace.get('treeref:scope');
    scope = seedScope === 'subtree' ? 'subtree' : 'exact';
    scopeSelect.setValue(scope);
    await loadFields();
    await loadRefFields();
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
