// file-manager panel: browse the disk (via metafolder.fs, not the
// daemon), distinguish tracked metarecords, add paths to the DB
// (spec-gui "file-manager panel type").

import { byId, el, fileTypeGlyph } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { latestOnly } from '/__coalesce.js';
import {
  loadTrackedChildren,
  loadDirMetarecord,
  parentDir,
  isWithin,
  entriesFooter,
  filterHidden,
  syntheticRows,
} from './tracked.js';
import {
  joinPath,
  dedupeName,
  baseName,
  getClipboard,
  setClipboard,
  hasClipboard,
  revealFolder,
} from '/__file-actions.js';

// The clipboard is shared across the whole GUI JS realm (see /__file-actions.js)
// — copy/cut here and paste in another file-manager, or in a metarecord panel's
// right-click menu, and back. Cleared after a cut is pasted (sources are gone).

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
  const { fs, daemon, workspace, commands, statusBar, bench, cache, trash } = metafolder;
  // Standard status-message duration (config.toml `[panels]`), with the former
  // hard-coded fallback.
  const statusMessageMs = metafolder.settings.statusMessageMs ?? 5000;
  const statusErrorMs = metafolder.settings.statusErrorMs ?? 8000;
  const PAGE = metafolder.pageSize ?? PAGE_DEFAULT;

  /** @type {string|null} */
  let repo = null;
  /** @type {string|null} */
  let repoRoot = null;
  /** @type {string|null} .metafolder/internal: hard-excluded from tracking */
  let internalDir = null;
  /** @type {string|null} */
  let currentDir = null;
  /** @type {Entry[]} the synthetic rows ("." / "..") then the directory's entries */
  let listing = [];
  // How many synthetic rows lead `listing` (2 with "." and "..", 1 when ".." is
  // dropped at the constrained repo root) — the footer subtracts these to count
  // the directory's real entries.
  let syntheticCount = 2;
  let rendered = 0; // number of `listing` rows currently in the DOM (windowed)
  let cursorIndex = -1;
  let constrainToRoot = true;
  let showHidden = false;
  /** @type {Map<string, string>} absolute path -> metarecord uuid (children of currentDir only) */
  let trackedPaths = new Map();
  /** The nonce of the last handled `file-manager:reveal-path` request, so the
   *  same request is never acted on twice (start + onChange).
   *  @type {unknown} */
  let revealNonce = null;

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
  // two synthetic rows always present in the first window. Returns the
  // directory's own metarecord uuid (its tree node), or null when untracked, so
  // the caller can fetch its children without resolving it again.
  /** @returns {Promise<string|null>} */
  async function enrichSelfParent() {
    const dir = currentDir;
    if (dir === null) return null;
    try {
      const parent = parentDir(dir);
      const [selfUuid, parentUuid] = await Promise.all([
        loadDirMetarecord(daemon, repo, repoRoot, dir),
        loadDirMetarecord(daemon, repo, repoRoot, parent),
      ]);
      if (selfUuid) trackedPaths.set(dir, selfUuid);
      if (parentUuid) trackedPaths.set(parent, parentUuid);
      return selfUuid;
    } catch (error) {
      await statusBar.error(error);
      return null;
    }
  }

  // Tracked status of every direct child of the current directory, fetched in
  // one `Follows` query the bitmap index serves (bounded to this directory's
  // children — not a per-window round-trip and not a full-repo scan). `dirUuid`
  // is the directory node resolved by `enrichSelfParent`.
  /** @param {string|null} dirUuid */
  async function enrichChildren(dirUuid) {
    const dir = currentDir;
    if (dir === null) return;
    try {
      const found = await loadTrackedChildren(daemon, repo, repoRoot, dir, dirUuid);
      for (const [path, uuid] of found) trackedPaths.set(path, uuid);
    } catch (error) {
      await statusBar.error(error);
    }
  }

  // Re-query the tracked status of the current directory (after a write, or an
  // external change): drop the stale map and refill it from the daemon.
  async function reenrichVisible() {
    trackedPaths = new Map();
    const dirUuid = await enrichSelfParent();
    await enrichChildren(dirUuid);
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
      await statusBar.error(error, statusErrorMs);
      return;
    }
    const synthetic = syntheticRows(dir, repoRoot, constrainToRoot);
    syntheticCount = synthetic.length;
    listing = [...synthetic, ...filterHidden(items, showHidden)];
    currentDir = dir;
    trackedPaths = new Map();
    cursorIndex = -1;
    rendered = Math.min(PAGE, listing.length);
    render(); // rows appear at once; tracked badges fill in just below
    const dirUuid = await enrichSelfParent();
    await enrichChildren(dirUuid);
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

    // Footer count excludes the synthetic "." / ".." rows, so it reflects
    // the directory's actual entries (mirrors metarecord-list).
    const total = Math.max(0, listing.length - syntheticCount);
    const shown = Math.max(0, Math.min(rendered, listing.length) - syntheticCount);
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
    // expands it so the row exists in the DOM to scroll to). Tracked status is
    // already loaded for the whole directory, so revealing rows needs no fetch.
    if (cursorIndex >= rendered) {
      rendered = cursorIndex + 1;
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

  // Right-click on a row: move the cursor there, then offer the row's actions
  // plus the directory-level create/paste actions.
  /** @param {MouseEvent} event @param {number} index */
  function rowMenu(event, index) {
    const item = listing[index];
    if (!item) return;
    void select(index);
    const isEntry = item.name !== '.' && item.name !== '..';
    /** @type {Metafolder.MenuItem[]} */
    const items = [];
    if (item.is_dir) items.push({ label: 'Open', action: () => void activate(index) }, '-');
    items.push({
      label: 'Track (mf_watch = false)',
      disabled: !repo || trackedPaths.has(item.path) || !trackable(item.path),
      action: () => void addSelected(),
    });
    if (isEntry) {
      items.push(
        '-',
        { label: 'Cut', action: () => clip('cut') },
        { label: 'Copy', action: () => clip('copy') },
        { label: 'Paste', disabled: !hasClipboard(), action: () => void paste() },
        '-',
        { label: 'Rename…', action: () => void renameSelected() },
        { label: 'Duplicate', action: () => void duplicateSelected() },
        { label: repo ? 'Move to trash' : 'Delete…', action: () => void deleteSelected() },
      );
    } else {
      items.push('-', { label: 'Paste', disabled: !hasClipboard(), action: () => void paste() });
    }
    items.push(
      '-',
      { label: 'New folder…', action: () => void newFolder() },
      { label: 'New file…', action: () => void newFile() },
    );
    metafolder.contextMenu(event, items);
  }

  // Right-click on empty space in the listing (not on a row): the directory-level
  // create/paste actions only.
  /** @param {MouseEvent} event */
  function backgroundMenu(event) {
    if (/** @type {Element} */ (event.target).closest('li')) return; // a row handles its own
    /** @type {Metafolder.MenuItem[]} */
    const items = [
      { label: 'New folder…', action: () => void newFolder() },
      { label: 'New file…', action: () => void newFile() },
      '-',
      { label: 'Paste', disabled: !hasClipboard(), action: () => void paste() },
    ];
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

  // ── Reveal a path (spec-gui "Cross-panel selection") ───────────────────────
  // Another panel asks the file manager to show a metarecord's folder: the
  // folder itself for a directory, or the containing folder (with the file
  // highlighted) for a file. The request travels through the workspace variable
  // `file-manager:reveal-path` = { path, nonce }, honoured both when the panel
  // mounts (start) and while it is already mounted (onChange). The `nonce` guard
  // makes an identical repeated request re-trigger, yet the same request act
  // only once across the two paths.

  /** Highlights the entry named `name` in the current listing, if present.
   *  @param {string} name */
  function selectByName(name) {
    const index = listing.findIndex((item) => item.name === name);
    if (index >= 0) void select(index);
  }

  /** Resolves a reveal request to the folder to open and the entry to
   *  highlight, statting the path to tell a directory from a file (a path that
   *  no longer exists is treated as a file, so its parent opens).
   *  @param {string} path @returns {Promise<{dir: string, select: string|null}>} */
  async function targetFor(path) {
    let isDir = false;
    try {
      isDir = !!(/** @type {{is_dir?: boolean}} */ (await fs.stat(path))).is_dir;
    } catch {
      /* gone: fall through as a file, so revealFolder opens the parent */
    }
    return revealFolder(path, isDir);
  }

  /** Reads a fresh `file-manager:reveal-path` request (marking its nonce
   *  handled) and resolves it, or null when there is none / already handled.
   *  @returns {Promise<{dir: string, select: string|null}|null>} */
  async function consumeReveal() {
    const req = await workspace.get('file-manager:reveal-path');
    if (!req || typeof req !== 'object') return null;
    const { path, nonce } = /** @type {{path?: unknown, nonce?: unknown}} */ (req);
    if (typeof path !== 'string' || !path || nonce === revealNonce) return null;
    revealNonce = nonce;
    return targetFor(path);
  }

  /** @param {{dir: string, select: string|null}} revealed */
  async function applyReveal(revealed) {
    if (constrainToRoot && repoRoot !== null && !insideRoot(revealed.dir)) {
      // The target lies outside the constrained root (a metarecord whose file is
      // outside the repo): honour the explicit request by dropping the
      // constraint rather than refusing to navigate.
      constrainToRoot = false;
      constrainBox.checked = false;
    }
    await open(revealed.dir);
    if (revealed.select) selectByName(revealed.select);
  }

  async function handleReveal() {
    const revealed = await consumeReveal();
    if (revealed) await applyReveal(revealed);
  }

  // ── Filesystem operations (spec-gui "file-manager panel type") ─────────────
  // The panel edits the disk directly, like it reads it — the daemon is never
  // involved, so a tracked metarecord goes stale until the watcher (mf_watch)
  // or a reconcile catches up (matching the panel's disk-only nature). The Rust
  // layer refuses to overwrite an existing destination, so a name clash is a
  // clear error, never a silent clobber.

  /** The names of the current directory's real entries (for de-duplication). */
  function takenNames() {
    const names = new Set();
    for (const item of listing) {
      if (item.name !== '.' && item.name !== '..') names.add(item.name);
    }
    return names;
  }

  /** The cursor's row, unless it is a synthetic "." / ".." row or nothing. */
  function selectedEntry() {
    const item = listing[cursorIndex];
    if (!item || item.name === '.' || item.name === '..') {
      void statusBar.message('select a file or folder first', 3000);
      return null;
    }
    return item;
  }

  // Reload the current directory after a mutation and move the cursor to
  // `selectName` when given (the freshly created/renamed entry). Other panels
  // showing this repo refresh via the metarecords:dirty nudge.
  /** @param {string|null} selectName */
  async function afterMutation(selectName) {
    await workspace.set('metarecords:dirty', Date.now());
    if (currentDir === null) return;
    await open(currentDir);
    if (selectName !== null) {
      const index = listing.findIndex((item) => item.name === selectName);
      if (index >= 0) await select(index);
    }
  }

  async function newFolder() {
    if (currentDir === null) return;
    const name = (prompt('New folder name:') ?? '').trim();
    if (!name) return;
    try {
      await fs.mkdir(joinPath(currentDir, name));
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    void statusBar.message(`Created folder ${name}`, statusMessageMs);
    await afterMutation(name);
  }

  async function newFile() {
    if (currentDir === null) return;
    const name = (prompt('New file name:') ?? '').trim();
    if (!name) return;
    try {
      await fs.createFile(joinPath(currentDir, name));
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    void statusBar.message(`Created file ${name}`, statusMessageMs);
    await afterMutation(name);
  }

  async function renameSelected() {
    if (currentDir === null) return;
    const item = selectedEntry();
    if (!item) return;
    const next = (prompt('Rename to:', item.name) ?? '').trim();
    if (!next || next === item.name) return;
    try {
      await fs.move(item.path, joinPath(currentDir, next));
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    void statusBar.message(`Renamed to ${next}`, statusMessageMs);
    await afterMutation(next);
  }

  async function duplicateSelected() {
    if (currentDir === null) return;
    const item = selectedEntry();
    if (!item) return;
    const name = dedupeName(item.name, takenNames());
    try {
      await fs.copy(item.path, joinPath(currentDir, name));
    } catch (error) {
      await statusBar.error(error, statusErrorMs);
      return;
    }
    void statusBar.message(`Duplicated to ${name}`, statusMessageMs);
    await afterMutation(name);
  }

  /** @param {'copy'|'cut'} mode */
  function clip(mode) {
    const item = selectedEntry();
    if (!item) return;
    setClipboard({ paths: [item.path], mode });
    void statusBar.message(`${mode === 'cut' ? 'Cut' : 'Copied'} ${item.name}`, statusMessageMs);
  }

  async function paste() {
    if (currentDir === null) return;
    const cb = getClipboard();
    if (!cb || cb.paths.length === 0) {
      void statusBar.message('clipboard is empty', 3000);
      return;
    }
    const { paths, mode } = cb;
    const taken = takenNames();
    let pasted = 0;
    for (const src of paths) {
      // Never move/copy a directory into itself or one of its descendants.
      if (isWithin(currentDir, src)) {
        await statusBar.error(`cannot paste ${baseName(src)} into itself`, statusErrorMs);
        continue;
      }
      const name = dedupeName(baseName(src), taken);
      const target = joinPath(currentDir, name);
      try {
        if (mode === 'cut') await fs.move(src, target);
        else await fs.copy(src, target);
        taken.add(name);
        pasted += 1;
      } catch (error) {
        await statusBar.error(error, statusErrorMs);
      }
    }
    if (mode === 'cut') setClipboard(null); // the sources have moved
    if (pasted > 0) {
      void statusBar.message(`Pasted ${pasted} item${pasted === 1 ? '' : 's'}`, statusMessageMs);
      await afterMutation(paths.length === 1 ? baseName(paths[0]) : null);
    }
  }

  async function deleteSelected() {
    if (currentDir === null) return;
    const item = selectedEntry();
    if (!item) return;
    // With an active repo, deletions are restorable through the repo trash;
    // otherwise (browsing the raw disk) there is nowhere to restore from, so
    // the removal is permanent and confirmed as such.
    if (repo) {
      if (!confirm(`Send "${item.name}" to the trash?`)) return;
      try {
        await trash.trashPath(repo, item.path);
      } catch (error) {
        await statusBar.error(error, statusErrorMs);
        return;
      }
      void statusBar.message(`Trashed ${item.name} — restore it from the trash panel`, statusMessageMs);
    } else {
      if (!confirm(`Permanently delete "${item.name}"? This cannot be undone.`)) return;
      try {
        await fs.remove(item.path);
      } catch (error) {
        await statusBar.error(error, statusErrorMs);
        return;
      }
      void statusBar.message(`Deleted ${item.name}`, statusMessageMs);
    }
    await afterMutation(null);
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

  // Reveal the next window of rows as the listing is scrolled to its end — the
  // shared progressive-loading controller owns the threshold and the
  // one-load-at-a-time guard. Tracked status needs no round-trip here: the
  // whole directory's tracked children were fetched once on open.
  const pager = createPagedList({
    loaded: () => rendered,
    total: () => listing.length,
    loadMore: async () => {
      rendered = Math.min(listing.length, rendered + PAGE);
      render();
    },
  });

  // Re-list the current directory so the ".." row (dis)appears immediately when
  // the constraint is toggled at the repo root.
  /** @param {boolean} checked */
  async function setConstrain(checked) {
    constrainToRoot = checked;
    constrainBox.checked = checked;
    if (currentDir !== null) await open(currentDir);
  }
  constrainBox.addEventListener('change', () => void setConstrain(constrainBox.checked));
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
    handler: () => setConstrain(!constrainToRoot),
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
  void commands.register('file-manager:new-folder', {
    label: 'File manager: create a new folder',
    handler: newFolder,
  });
  void commands.register('file-manager:new-file', {
    label: 'File manager: create a new empty file',
    handler: newFile,
  });
  void commands.register('file-manager:rename', {
    label: 'File manager: rename the selected entry',
    handler: renameSelected,
  });
  void commands.register('file-manager:duplicate', {
    label: 'File manager: duplicate the selected entry',
    handler: duplicateSelected,
  });
  void commands.register('file-manager:copy', {
    label: 'File manager: copy the selected entry to the clipboard',
    handler: () => clip('copy'),
  });
  void commands.register('file-manager:cut', {
    label: 'File manager: cut the selected entry to the clipboard',
    handler: () => clip('cut'),
  });
  void commands.register('file-manager:paste', {
    label: 'File manager: paste the clipboard into the current directory',
    handler: paste,
  });
  void commands.register('file-manager:delete', {
    label: 'File manager: delete the selected entry (trash when a repo is active)',
    handler: deleteSelected,
  });

  listingElement.addEventListener('contextmenu', backgroundMenu);

  // Keybindings for this panel live in keybindings.toml (when = "file-manager").

  async function start() {
    repo = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
    constrainBox.disabled = repo === null;
    // A value picker (spec-gui "Value picker") can seed the directory to open
    // at — e.g. the repos panel's folder picker starts from the typed path.
    const seedDir = await workspace.get('file-manager:start-dir');
    const seedStart = typeof seedDir === 'string' && seedDir ? seedDir : null;
    if (repo !== null) {
      await cache.sync(repo); // fresh tracked status on display
      repoRoot = await daemon.repoRoot(repo);
      internalDir = await daemon.repoInternalDir(repo);
    } else {
      // No repo: everything is untracked; browse from the reveal/seed or root.
      repoRoot = null;
      internalDir = null;
      placeholderElement.textContent = 'No active repository — browsing the disk.';
    }
    // A pending reveal request (from another panel's "open folder" action) wins
    // over the plain seed; otherwise fall back to the seed, the repo root, or /.
    const revealed = await consumeReveal();
    if (revealed) await applyReveal(revealed);
    else await open(seedStart ?? repoRoot ?? '/');
  }

  // The first directory listing waits for the first actual display.
  const deferredStart = () => void start();
  workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
  // A reveal request while the panel is already mounted: navigate to the folder
  // when it next becomes visible (start() handles a request present at mount).
  workspace.onChange('file-manager:reveal-path', () =>
    metafolder.whenVisible(() => void handleReveal()),
  );
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
