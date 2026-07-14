// file-manager panel: browse the disk (via metafolder.fs, not the
// daemon), distinguish tracked metarecords, add paths to the DB
// (spec-gui "file-manager panel type").

import { byId, el, fileTypeGlyph } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { latestOnly } from '/__coalesce.js';
import {
  loadTrackedFor,
  loadDirMetarecord,
  parentDir,
  isWithin,
  entriesFooter,
  filterHidden,
} from './tracked.js';

// Render the directory in windows of this many rows (plus more on scroll), so
// a directory with thousands of entries does not build a huge DOM — and, just
// as importantly, only the rendered window's tracked status is queried. The
// size comes from the GUI config (`[page-size].file-manager`); this is only
// the fallback when no config value is provided.
const PAGE_DEFAULT = 200;

/**
 * One row of the listing: a real directory entry, or the synthetic "." / ".."
 * rows this panel prepends.
 * @typedef {Metafolder.FsEntry} Entry
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { fs, daemon, workspace, commands, statusBar, bench, cache } = metafolder;
  // Standard status-message duration (config.toml `[panels]`), with the former
  // hard-coded fallback.
  const statusMessageMs = metafolder.settings.statusMessageMs ?? 5000;
  const PAGE = metafolder.pageSize ?? PAGE_DEFAULT;

  /** @type {string|null} */
  let repo = null;
  /** @type {string|null} */
  let repoRoot = null;
  /** @type {string|null} .metafolder/internal: hard-excluded from tracking */
  let internalDir = null;
  /** @type {string|null} */
  let currentDir = null;
  /** @type {Entry[]} "." and ".." then the directory's entries */
  let listing = [];
  let rendered = 0; // number of `listing` rows currently in the DOM (windowed)
  let cursorIndex = -1;
  let constrainToRoot = true;
  let showHidden = false;
  /** @type {Map<string, string>} absolute path -> metarecord uuid (children of currentDir only) */
  let trackedPaths = new Map();

  const entriesList = byId(root, 'entries');
  const placeholderElement = byId(root, 'placeholder');
  const pathElement = byId(root, 'current-path');
  const addButton = byId(root, 'add', HTMLButtonElement);
  const constrainBox = byId(root, 'constrain', HTMLInputElement);
  const showHiddenBox = byId(root, 'show-hidden', HTMLInputElement);
  const statusLine = byId(root, 'status-line');
  const listingElement = byId(root, 'listing');

  /** @param {string} path */
  function insideRoot(path) {
    return isWithin(path, repoRoot);
  }

  /** @param {string} path */
  function trackable(path) {
    return insideRoot(path) && !isWithin(path, internalDir);
  }

  // Tracked status of "." (the directory itself) and ".." (its parent) — the
  // two synthetic rows always present in the first window.
  async function enrichSelfParent() {
    const dir = currentDir;
    if (dir === null) return;
    try {
      const parent = parentDir(dir);
      const [selfUuid, parentUuid] = await Promise.all([
        loadDirMetarecord(daemon, repo, repoRoot, dir),
        loadDirMetarecord(daemon, repo, repoRoot, parent),
      ]);
      if (selfUuid) trackedPaths.set(dir, selfUuid);
      if (parentUuid) trackedPaths.set(parent, parentUuid);
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Tracked status of the real entries in listing[start, end) — queried for
  // just that window's names, so a huge directory never resolves every child.
  /** @param {number} start @param {number} end */
  async function enrichRange(start, end) {
    const dir = currentDir;
    if (dir === null) return;
    /** @type {string[]} */
    const names = [];
    for (let i = start; i < end; i++) {
      const item = listing[i];
      if (item && item.name !== '.' && item.name !== '..') names.push(item.name);
    }
    try {
      const found = await loadTrackedFor(daemon, repo, repoRoot, dir, names);
      for (const [path, uuid] of found) trackedPaths.set(path, uuid);
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Re-query the tracked status of everything currently rendered (after a
  // write, or an external change): drop the stale map and refill the window.
  async function reenrichVisible() {
    trackedPaths = new Map();
    await enrichSelfParent();
    await enrichRange(0, rendered);
  }

  /** @param {string} dir @returns {Promise<void>} */
  function open(dir) {
    return bench.measure('mf:fm:load', () => openNow(dir));
  }

  /** @param {string} dir */
  async function openNow(dir) {
    if (constrainToRoot && repoRoot !== null && !insideRoot(dir)) {
      void statusBar.message('navigation is constrained to the repo root', 4000);
      return;
    }
    let items;
    try {
      items = await fs.readDir(dir);
    } catch (error) {
      await statusBar.error(error, statusMessageMs);
      return;
    }
    listing = [
      { name: '.', path: dir, is_dir: true },
      { name: '..', path: parentDir(dir), is_dir: true },
      ...filterHidden(items, showHidden),
    ];
    currentDir = dir;
    trackedPaths = new Map();
    cursorIndex = -1;
    rendered = Math.min(PAGE, listing.length);
    render(); // rows appear at once; tracked badges fill in just below
    await enrichSelfParent();
    await enrichRange(0, rendered);
    render();
  }

  function render() {
    bench.measure('mf:fm:render', renderNow);
  }

  function renderNow() {
    pathElement.textContent = currentDir ?? '';
    placeholderElement.hidden = true;
    const selected = listing[cursorIndex];
    addButton.disabled =
      !repo || !selected || trackedPaths.has(selected.path) || !trackable(selected.path);

    entriesList.replaceChildren(
      ...listing.slice(0, rendered).map((item, index) => {
        const internal = isWithin(item.path, internalDir);
        return el(
          'li',
          {
            class: [
              index === cursorIndex && 'cursor',
              trackedPaths.has(item.path) && 'tracked',
              internal && 'internal',
            ],
            onclick: () => select(index),
            ondblclick: () => activate(index),
            oncontextmenu: (/** @type {MouseEvent} */ event) => rowMenu(event, index),
            ...(internal && { title: 'always excluded from tracking (live database)' }),
          },
          el('span', { class: 'icon' }, item.is_dir ? '📁' : fileTypeGlyph(item.name)),
          el('span', { class: 'name' }, item.name),
          el(
            'span',
            { class: 'badge' },
            internal ? 'internal' : trackedPaths.has(item.path) ? 'tracked' : '',
          ),
        );
      }),
    );

    // Footer count excludes the synthetic "." and ".." rows, so it reflects
    // the directory's actual entries (mirrors metarecord-list).
    const total = Math.max(0, listing.length - 2);
    const shown = Math.max(0, Math.min(rendered, listing.length) - 2);
    statusLine.textContent = entriesFooter(shown, total);
  }

  // Held-arrow navigation must stay cheaper than the key-repeat rate, or key
  // events accumulate and keep replaying after release: inside the rendered
  // window moving the cursor only retargets the `.cursor` class (the rows are
  // index-aligned with `listing`) and refreshes the add button; the selection
  // propagation (two workspace.set IPC round-trips fanning out to the other
  // panels) is coalesced — one in flight, one trailing with the final position.
  function moveCursorHighlight() {
    entriesList.querySelector('.cursor')?.classList.remove('cursor');
    entriesList.children[cursorIndex]?.classList.add('cursor');
    const selected = listing[cursorIndex];
    addButton.disabled =
      !repo || !selected || trackedPaths.has(selected.path) || !trackable(selected.path);
  }

  const propagateSelection = latestOnly(async () => {
    const item = listing[cursorIndex];
    if (!item) return;
    await workspace.set('selected_paths', [item.path]);
    const uuid = trackedPaths.get(item.path);
    await workspace.set('selected_metarecord', uuid ? { uuid, repo } : null);
  });

  /** @param {number} index */
  async function select(index) {
    cursorIndex = Math.max(0, Math.min(index, listing.length - 1));
    // Keep the cursor inside the rendered window (jumping to the last entry
    // expands it so the row exists in the DOM to scroll to), enriching the
    // newly revealed rows.
    if (cursorIndex >= rendered) {
      const prev = rendered;
      rendered = cursorIndex + 1;
      render();
      await enrichRange(prev, rendered);
      render();
    } else {
      moveCursorHighlight();
    }
    if (!listing[cursorIndex]) return;
    root.querySelector('li.cursor')?.scrollIntoView({ block: 'nearest' });
    await propagateSelection();
  }

  /** @param {number} index */
  async function activate(index) {
    const item = listing[index];
    if (!item) return;
    if (item.is_dir) await open(item.path);
    else await select(index);
  }

  // Right-click on a row: move the cursor there, then offer the row's actions.
  /** @param {MouseEvent} event @param {number} index */
  function rowMenu(event, index) {
    const item = listing[index];
    if (!item) return;
    void select(index);
    /** @type {Metafolder.MenuItem[]} */
    const items = [];
    if (item.is_dir) items.push({ label: 'Open', action: () => void activate(index) }, '-');
    items.push({
      label: 'Track (mf_watch = false)',
      disabled: !repo || trackedPaths.has(item.path) || !trackable(item.path),
      action: () => void addSelected(),
    });
    metafolder.contextMenu(event, items);
  }

  async function goUp() {
    if (!currentDir || currentDir === '/') return;
    await open(parentDir(currentDir));
  }

  async function addSelected() {
    const item = listing[cursorIndex];
    if (!repo) {
      void statusBar.message('no active repository', 4000);
      return;
    }
    if (!item) return;
    try {
      await daemon.call('POST', `/repos/${repo}/track`, { path: item.path });
    } catch (error) {
      await statusBar.error(error, 6000);
      return;
    }
    void statusBar.message(`Tracked: ${item.name} (mf_watch = false)`, statusMessageMs);
    await reenrichVisible();
    render();
    await select(cursorIndex);
    await workspace.set('metarecords:dirty', Date.now());
  }

  async function gotoRoot() {
    if (!repo || repoRoot === null) {
      void statusBar.message('no active repository', 4000);
      return;
    }
    await open(repoRoot);
  }

  // Re-read the current directory and its tracked status from disk/daemon,
  // preserving the cursor position.
  async function refresh() {
    if (currentDir === null) return;
    const keep = cursorIndex;
    if (repo) await cache.sync(repo);
    await open(currentDir);
    if (keep >= 0) await select(keep);
  }

  // Reveal (and enrich) the next window of rows as the listing is scrolled to
  // its end — the shared progressive-loading controller owns the threshold and
  // the one-load-at-a-time guard.
  const pager = createPagedList({
    loaded: () => rendered,
    total: () => listing.length,
    loadMore: async () => {
      const prev = rendered;
      rendered = Math.min(listing.length, rendered + PAGE);
      render();
      await enrichRange(prev, rendered);
      render();
    },
  });

  constrainBox.addEventListener('change', () => {
    constrainToRoot = constrainBox.checked;
  });
  // Re-list the current directory so the dot-entries (dis)appear immediately.
  /** @param {boolean} shown */
  async function setShowHidden(shown) {
    showHidden = shown;
    showHiddenBox.checked = shown;
    if (currentDir !== null) await open(currentDir);
  }
  showHiddenBox.addEventListener('change', () => void setShowHidden(showHiddenBox.checked));
  const detachScroll = pager.attach(listingElement);
  byId(root, 'up').addEventListener('click', () => void goUp());
  byId(root, 'goto-root').addEventListener('click', () => void gotoRoot());
  byId(root, 'refresh').addEventListener('click', () => void refresh());
  addButton.addEventListener('click', () => void addSelected());

  void commands.register('file-manager:add', {
    label: 'File manager: track the selected path (mf_watch = false)',
    handler: addSelected,
  });
  void commands.register('file-manager:goto-root', {
    label: 'File manager: jump to the repo root',
    handler: gotoRoot,
  });
  void commands.register('file-manager:refresh', {
    label: 'File manager: reload the current directory',
    handler: refresh,
  });
  void commands.register('file-manager:toggle-root', {
    label: 'File manager: toggle the root constraint',
    handler: () => {
      constrainBox.checked = !constrainBox.checked;
      constrainToRoot = constrainBox.checked;
    },
  });
  void commands.register('file-manager:toggle-hidden', {
    label: 'File manager: show/hide hidden files (dot-entries)',
    handler: () => setShowHidden(!showHidden),
  });
  void commands.register('file-manager:next', {
    label: 'File manager: move down',
    handler: () => select(cursorIndex + 1),
  });
  void commands.register('file-manager:prev', {
    label: 'File manager: move up',
    handler: () => select(cursorIndex - 1),
  });
  void commands.register('file-manager:first', {
    label: 'File manager: move to the first entry',
    handler: () => select(0),
  });
  void commands.register('file-manager:last', {
    label: 'File manager: move to the last entry',
    handler: () => select(listing.length - 1),
  });
  void commands.register('file-manager:activate', {
    label: 'File manager: open directory / confirm file',
    handler: () => activate(cursorIndex),
  });
  void commands.register('file-manager:parent', {
    label: 'File manager: go up one level',
    handler: goUp,
  });

  // Keybindings for this panel live in keybindings.toml (when = "file-manager").

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    constrainBox.disabled = repo === null;
    // A value picker (spec-gui "Value picker") can seed the directory to open
    // at — e.g. the repos panel's folder picker starts from the typed path.
    const seedDir = await workspace.get('file-manager:start-dir');
    const start = typeof seedDir === 'string' && seedDir ? seedDir : null;
    if (repo !== null) {
      await cache.sync(repo); // fresh tracked status on display
      repoRoot = await daemon.repoRoot(repo);
      internalDir = await daemon.repoInternalDir(repo);
      await open(start ?? repoRoot);
    } else {
      // No repo: browse from the seed (or the root), everything untracked.
      repoRoot = null;
      internalDir = null;
      placeholderElement.textContent = 'No active repository — browsing the disk.';
      await open(start ?? '/');
    }
  }

  // The first directory listing waits for the first actual display.
  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  async function onMetarecordsDirty() {
    if (currentDir === null) return; // not started yet (still hidden)
    if (repo) await cache.sync(repo); // pick up the change before re-querying
    await reenrichVisible();
    render();
  }
  workspace.onChange('metarecords:dirty', () => void onMetarecordsDirty());

  metafolder.whenVisible(deferredStart);

  return () => detachScroll();
}
