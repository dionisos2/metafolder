// file-manager panel: browse the disk (via metafolder.fs, not the
// daemon), distinguish tracked entries, add paths to the DB
// (spec-gui "file-manager panel type").

import { el } from '/__ui.js';

const { fs, daemon, workspace, commands, statusBar } = metafolder;

let repo = null;
let repoRoot = null;
let currentDir = null;
let listing = []; // [{name, path, is_dir}]
let cursorIndex = -1;
let constrainToRoot = true;
let trackedPaths = new Map(); // absolute path -> entry uuid

const entriesList = document.getElementById('entries');
const placeholderElement = document.getElementById('placeholder');
const pathElement = document.getElementById('current-path');
const addButton = document.getElementById('add');
const constrainBox = document.getElementById('constrain');

function insideRoot(path) {
  return repoRoot !== null && (path === repoRoot || path.startsWith(`${repoRoot}/`));
}

// All tracked absolute paths of the repo, resolved once per refresh
// (paginated query on mfr_path; fine for v1 sizes).
async function loadTrackedPaths() {
  trackedPaths = new Map();
  if (!repo) return;
  let cursor = null;
  do {
    let page;
    try {
      page = await daemon.call('POST', `/repos/${repo}/query`, {
        query: { type: 'is_present', field: 'mfr_path' },
        select: '*',
        limit: 500,
        ...(cursor && { cursor }),
      });
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    for (const entry of page.results) {
      for (const path of await daemon.entryPaths(repo, entry)) {
        trackedPaths.set(path, entry.uuid);
      }
    }
    cursor = page.next_cursor;
  } while (cursor);
}

async function open(dir) {
  if (constrainToRoot && repoRoot !== null && !insideRoot(dir)) {
    statusBar.message('navigation is constrained to the repo root', 4000);
    return;
  }
  try {
    listing = await fs.readDir(dir);
    currentDir = dir;
    cursorIndex = -1;
    render();
  } catch (error) {
    await statusBar.error(error, 5000);
  }
}

function render() {
  pathElement.textContent = currentDir ?? '';
  placeholderElement.hidden = true;
  const selected = listing[cursorIndex];
  addButton.disabled =
    !repo || !selected || trackedPaths.has(selected.path) || !insideRoot(selected.path);

  entriesList.replaceChildren(
    ...listing.map((item, index) =>
      el(
        'li',
        {
          class: [index === cursorIndex && 'cursor', trackedPaths.has(item.path) && 'tracked'],
          onclick: () => select(index),
          ondblclick: () => activate(index),
        },
        el('span', { class: 'icon' }, item.is_dir ? '▸' : '·'),
        el('span', { class: 'name' }, item.name),
        el('span', { class: 'badge' }, trackedPaths.has(item.path) ? 'tracked' : ''),
      ),
    ),
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
  await workspace.set('selected_entry', uuid ? { uuid, repo } : null);
}

async function activate(index) {
  const item = listing[index];
  if (!item) return;
  if (item.is_dir) await open(item.path);
  else await select(index);
}

async function goUp() {
  if (!currentDir || currentDir === '/') return;
  const parent = currentDir.slice(0, currentDir.lastIndexOf('/')) || '/';
  await open(parent);
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
  await loadTrackedPaths();
  render();
  await select(cursorIndex);
  await workspace.set('entries:dirty', Date.now());
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
metafolder.addKeybinding('file-manager:activate', 'enter');
metafolder.addKeybinding('file-manager:parent', 'backspace');

async function start() {
  repo = await workspace.get('active_repo');
  constrainBox.disabled = repo === null;
  if (repo !== null) {
    repoRoot = await daemon.repoRoot(repo);
    await loadTrackedPaths();
    await open(repoRoot);
  } else {
    // No repo: browse from the home directory, everything untracked.
    placeholderElement.textContent = 'No active repository — browsing the disk.';
    await open('/');
  }
}

workspace.onChange('active_repo', () => void start());
workspace.onChange('entries:dirty', async () => {
  await loadTrackedPaths();
  render();
});

await start();
