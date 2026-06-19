// file-manager panel: browse the disk (via metafolder.fs, not the
// daemon), distinguish tracked metarecords, add paths to the DB
// (spec-gui "file-manager panel type").

import { el } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import {
  loadTrackedFor,
  loadDirMetarecord,
  parentDir,
  isWithin,
  entriesFooter,
} from './tracked.js';

// Render the directory in windows of this many rows (plus more on scroll), so
// a directory with thousands of entries does not build a huge DOM — and, just
// as importantly, only the rendered window's tracked status is queried. The
// size comes from the GUI config (`[page-size].file-manager`); this is only
// the fallback when no config value is provided.
const PAGE_DEFAULT = 200;

export async function mount(root, metafolder) {
  const { fs, daemon, workspace, commands, statusBar, bench, cache } = metafolder;
  const PAGE = metafolder.pageSize ?? PAGE_DEFAULT;

  let repo = null;
  let repoRoot = null;
  let internalDir = null; // .metafolder/internal: hard-excluded from tracking
  let currentDir = null;
  let listing = []; // [{name, path, is_dir}] — "." and ".." then the entries
  let rendered = 0; // number of `listing` rows currently in the DOM (windowed)
  let cursorIndex = -1;
  let constrainToRoot = true;
  let trackedPaths = new Map(); // absolute path -> metarecord uuid (children of currentDir only)

  const entriesList = root.getElementById('entries');
  const placeholderElement = root.getElementById('placeholder');
  const pathElement = root.getElementById('current-path');
  const addButton = root.getElementById('add');
  const constrainBox = root.getElementById('constrain');
  const statusLine = root.getElementById('status-line');
  const listingElement = root.getElementById('listing');

  function insideRoot(path) {
    return isWithin(path, repoRoot);
  }

  function trackable(path) {
    return insideRoot(path) && !isWithin(path, internalDir);
  }

  // Tracked status of "." (the directory itself) and ".." (its parent) — the
  // two synthetic rows always present in the first window.
  async function enrichSelfParent() {
    try {
      const parent = parentDir(currentDir);
      const [selfUuid, parentUuid] = await Promise.all([
        loadDirMetarecord(daemon, repo, repoRoot, currentDir),
        loadDirMetarecord(daemon, repo, repoRoot, parent),
      ]);
      if (selfUuid) trackedPaths.set(currentDir, selfUuid);
      if (parentUuid) trackedPaths.set(parent, parentUuid);
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Tracked status of the real entries in listing[start, end) — queried for
  // just that window's names, so a huge directory never resolves every child.
  async function enrichRange(start, end) {
    const names = [];
    for (let i = start; i < end; i++) {
      const item = listing[i];
      if (item && item.name !== '.' && item.name !== '..') names.push(item.name);
    }
    try {
      const found = await loadTrackedFor(daemon, repo, repoRoot, currentDir, names);
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

  function open(dir) {
    return bench.measure('mf:fm:load', () => openNow(dir));
  }

  async function openNow(dir) {
    if (constrainToRoot && repoRoot !== null && !insideRoot(dir)) {
      statusBar.message('navigation is constrained to the repo root', 4000);
      return;
    }
    let items;
    try {
      items = await fs.readDir(dir);
    } catch (error) {
      await statusBar.error(error, 5000);
      return;
    }
    listing = [
      { name: '.', path: dir, is_dir: true },
      { name: '..', path: parentDir(dir), is_dir: true },
      ...items,
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
            oncontextmenu: (event) => rowMenu(event, index),
            ...(internal && { title: 'always excluded from tracking (live database)' }),
          },
          el('span', { class: 'icon' }, item.is_dir ? '▸' : '·'),
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
    }
    render();
    const item = listing[cursorIndex];
    if (!item) return;
    root.querySelector('li.cursor')?.scrollIntoView({ block: 'nearest' });
    await workspace.set('selected_paths', [item.path]);
    const uuid = trackedPaths.get(item.path);
    await workspace.set('selected_metarecord', uuid ? { uuid, repo } : null);
  }

  async function activate(index) {
    const item = listing[index];
    if (!item) return;
    if (item.is_dir) await open(item.path);
    else await select(index);
  }

  // Right-click on a row: move the cursor there, then offer the row's actions.
  function rowMenu(event, index) {
    const item = listing[index];
    if (!item) return;
    void select(index);
    void metafolder.contextMenu(
      event,
      [
        item.is_dir && { label: 'Open', action: () => void activate(index) },
        item.is_dir && '-',
        {
          label: 'Track (mf_watch = false)',
          disabled: !repo || trackedPaths.has(item.path) || !trackable(item.path),
          action: () => void addSelected(),
        },
      ].filter(Boolean),
    );
  }

  async function goUp() {
    if (!currentDir || currentDir === '/') return;
    await open(parentDir(currentDir));
  }

  async function addSelected() {
    const item = listing[cursorIndex];
    if (!repo) {
      statusBar.message('no active repository', 4000);
      return;
    }
    if (!item) return;
    try {
      await daemon.call('POST', `/repos/${repo}/track`, { path: item.path });
    } catch (error) {
      await statusBar.error(error, 6000);
      return;
    }
    statusBar.message(`Tracked: ${item.name} (mf_watch = false)`, 5000);
    await reenrichVisible();
    render();
    await select(cursorIndex);
    await workspace.set('metarecords:dirty', Date.now());
  }

  async function gotoRoot() {
    if (!repo || repoRoot === null) {
      statusBar.message('no active repository', 4000);
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
  const detachScroll = pager.attach(listingElement);
  root.getElementById('up').addEventListener('click', goUp);
  root.getElementById('goto-root').addEventListener('click', gotoRoot);
  root.getElementById('refresh').addEventListener('click', refresh);
  addButton.addEventListener('click', addSelected);

  commands.register('file-manager:add', {
    label: 'File manager: track the selected path (mf_watch = false)',
    handler: addSelected,
  });
  commands.register('file-manager:goto-root', {
    label: 'File manager: jump to the repo root',
    handler: gotoRoot,
  });
  commands.register('file-manager:refresh', {
    label: 'File manager: reload the current directory',
    handler: refresh,
  });
  commands.register('file-manager:toggle-root', {
    label: 'File manager: toggle the root constraint',
    handler: () => {
      constrainBox.checked = !constrainBox.checked;
      constrainToRoot = constrainBox.checked;
    },
  });
  commands.register('file-manager:next', {
    label: 'File manager: move down',
    handler: () => select(cursorIndex + 1),
  });
  commands.register('file-manager:prev', {
    label: 'File manager: move up',
    handler: () => select(cursorIndex - 1),
  });
  commands.register('file-manager:first', {
    label: 'File manager: move to the first entry',
    handler: () => select(0),
  });
  commands.register('file-manager:last', {
    label: 'File manager: move to the last entry',
    handler: () => select(listing.length - 1),
  });
  commands.register('file-manager:activate', {
    label: 'File manager: open directory / confirm file',
    handler: () => activate(cursorIndex),
  });
  commands.register('file-manager:parent', {
    label: 'File manager: go up one level',
    handler: goUp,
  });

  metafolder.addKeybinding('file-manager:next', 'down');
  metafolder.addKeybinding('file-manager:next', 'j');
  metafolder.addKeybinding('file-manager:prev', 'up');
  metafolder.addKeybinding('file-manager:prev', 'k');
  metafolder.addKeybinding('file-manager:first', 'home');
  metafolder.addKeybinding('file-manager:last', 'end');
  metafolder.addKeybinding('file-manager:activate', 'enter');
  metafolder.addKeybinding('file-manager:parent', 'backspace');
  metafolder.addKeybinding('file-manager:refresh', 'r');

  async function start() {
    repo = await workspace.get('active_repo');
    constrainBox.disabled = repo === null;
    if (repo !== null) {
      await cache.sync(repo); // fresh tracked status on display
      repoRoot = await daemon.repoRoot(repo);
      internalDir = await daemon.repoInternalDir(repo);
      await open(repoRoot);
    } else {
      // No repo: browse from the root, everything untracked.
      repoRoot = null;
      internalDir = null;
      placeholderElement.textContent = 'No active repository — browsing the disk.';
      await open('/');
    }
  }

  // The first directory listing waits for the first actual display.
  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  workspace.onChange('metarecords:dirty', async () => {
    if (currentDir === null) return; // not started yet (still hidden)
    if (repo) await cache.sync(repo); // pick up the change before re-querying
    await reenrichVisible();
    render();
  });

  metafolder.whenVisible(deferredStart);

  return () => detachScroll();
}
