// file-manager panel: browse the disk (via metafolder.fs, not the
// daemon), distinguish tracked metarecords, add paths to the DB
// (spec-gui "file-manager panel type").

import { el } from '/__ui.js';
import { loadTrackedChildren, loadDirMetarecord, parentDir, isWithin } from './tracked.js';

const { fs, daemon, workspace, commands, statusBar } = metafolder;

let repo = null;
let repoRoot = null;
let internalDir = null; // .metafolder/internal: hard-excluded from tracking
let currentDir = null;
let listing = []; // [{name, path, is_dir}]
let cursorIndex = -1;
let constrainToRoot = true;
let trackedPaths = new Map(); // absolute path -> metarecord uuid (children of currentDir only)

const entriesList = document.getElementById('entries');
const placeholderElement = document.getElementById('placeholder');
const pathElement = document.getElementById('current-path');
const addButton = document.getElementById('add');
const constrainBox = document.getElementById('constrain');

function insideRoot(path) {
  return isWithin(path, repoRoot);
}

function trackable(path) {
  return insideRoot(path) && !isWithin(path, internalDir);
}

// Tracked status of the displayed directory's children plus the "." and
// ".." rows (tracked.js); the rest of the repo is never fetched.
async function refreshTracked(dir) {
  try {
    const parent = parentDir(dir);
    const [tracked, selfUuid, parentUuid] = await Promise.all([
      loadTrackedChildren(daemon, repo, repoRoot, dir),
      loadDirMetarecord(daemon, repo, repoRoot, dir),
      loadDirMetarecord(daemon, repo, repoRoot, parent),
    ]);
    if (selfUuid) tracked.set(dir, selfUuid);
    if (parentUuid) tracked.set(parent, parentUuid);
    trackedPaths = tracked;
  } catch (error) {
    trackedPaths = new Map();
    await statusBar.error(error);
  }
}

async function open(dir) {
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
  await refreshTracked(dir);
  listing = [
    { name: '.', path: dir, is_dir: true },
    { name: '..', path: parentDir(dir), is_dir: true },
    ...items,
  ];
  currentDir = dir;
  cursorIndex = -1;
  render();
}

function render() {
  pathElement.textContent = currentDir ?? '';
  placeholderElement.hidden = true;
  const selected = listing[cursorIndex];
  addButton.disabled =
    !repo || !selected || trackedPaths.has(selected.path) || !trackable(selected.path);

  entriesList.replaceChildren(
    ...listing.map((item, index) => {
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
}

async function select(index) {
  cursorIndex = Math.max(0, Math.min(index, listing.length - 1));
  render();
  const item = listing[cursorIndex];
  if (!item) return;
  document.querySelector('li.cursor')?.scrollIntoView({ block: 'nearest' });
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

// Right-click on a row: move the cursor there, then offer the row's
// actions as an HTML context menu (the native menu is suppressed).
function rowMenu(event, index) {
  const item = listing[index];
  if (!item) return;
  void select(index);
  void metafolder.contextMenu(event, [
    item.is_dir && { label: 'Open', action: () => void activate(index) },
    item.is_dir && '-',
    {
      label: 'Track (mf_watch = false)',
      disabled: !repo || trackedPaths.has(item.path) || !trackable(item.path),
      action: () => void addSelected(),
    },
  ].filter(Boolean));
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
  await refreshTracked(currentDir);
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

constrainBox.addEventListener('change', () => {
  constrainToRoot = constrainBox.checked;
});
document.getElementById('up').addEventListener('click', goUp);
document.getElementById('goto-root').addEventListener('click', gotoRoot);
addButton.addEventListener('click', addSelected);

await metafolder.ready;

commands.register('file-manager:add', {
  label: 'File manager: track the selected path (mf_watch = false)',
  handler: addSelected,
});
commands.register('file-manager:goto-root', {
  label: 'File manager: jump to the repo root',
  handler: gotoRoot,
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

async function start() {
  repo = await workspace.get('active_repo');
  constrainBox.disabled = repo === null;
  if (repo !== null) {
    repoRoot = await daemon.repoRoot(repo);
    internalDir = await daemon.repoInternalDir(repo);
    await open(repoRoot);
  } else {
    // No repo: browse from the home directory, everything untracked.
    repoRoot = null;
    internalDir = null;
    placeholderElement.textContent = 'No active repository — browsing the disk.';
    await open('/');
  }
}

// The first directory listing waits for the first actual display:
// construction stays cheap so the panel type can be pre-instantiated
// hidden (commands registered, no filesystem/daemon traffic).
const deferredStart = () => void start();
workspace.onChange('active_repo', () => metafolder.whenVisible(deferredStart));
workspace.onChange('metarecords:dirty', async () => {
  if (currentDir === null) return; // not started yet (still hidden)
  await refreshTracked(currentDir);
  render();
});

metafolder.whenVisible(deferredStart);
