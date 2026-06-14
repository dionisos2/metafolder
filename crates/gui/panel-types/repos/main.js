// repos panel: list loaded repositories, init/load new ones, open a
// repository in a workspace (spec-gui "Repository management").

import { el } from '/__ui.js';

const { daemon, workspace, commands, statusBar, fs } = metafolder;

const list = document.getElementById('repo-list');
const empty = document.getElementById('empty');
const initForm = document.getElementById('init-form');
const loadForm = document.getElementById('load-form');

function toggleForm(form, show) {
  form.classList.toggle('hidden', !show);
  if (!show) closeBrowser();
  if (show) form.querySelector('input').focus();
}

// ── Folder picker ─────────────────────────────────────────────────────────────
// An in-panel directory browser (no native dialog): navigate with
// metafolder.fs.readDir and fill the target form input with the chosen
// folder, so the root path need not be typed by hand.

const browser = document.getElementById('browser');
const browserList = document.getElementById('browser-list');
const browserPath = document.getElementById('browser-path');
let browserDir = '/';
let browserTarget = null; // the <input> to fill on "Use this folder"

function parentDir(path) {
  const index = path.lastIndexOf('/');
  return index <= 0 ? '/' : path.slice(0, index);
}

function basename(path) {
  return path.split('/').filter(Boolean).pop() ?? path;
}

async function browseTo(dir) {
  let entries;
  try {
    entries = await fs.readDir(dir);
  } catch (error) {
    // Unreadable target (e.g. a stale input value): fall back to the root.
    if (dir !== '/') {
      await browseTo('/');
      return;
    }
    await statusBar.error(error);
    return;
  }
  browserDir = dir;
  browserPath.textContent = dir;
  const dirs = entries.filter((entry) => entry.is_dir);
  if (dirs.length === 0) {
    browserList.replaceChildren(el('li', { class: 'browser-empty' }, '(no subfolders)'));
    return;
  }
  browserList.replaceChildren(
    ...dirs.map((entry) =>
      el(
        'li',
        { onclick: () => void browseTo(entry.path) },
        el('span', { class: 'icon' }, '📁'),
        el('span', { class: 'name' }, entry.name),
      ),
    ),
  );
}

function openBrowser(targetInput) {
  browserTarget = targetInput;
  browser.classList.remove('hidden');
  const start = targetInput.value.trim();
  void browseTo(start || '/');
}

function closeBrowser() {
  browser.classList.add('hidden');
  browserTarget = null;
}

function pickFolder() {
  if (browserTarget) {
    browserTarget.value = browserDir;
    // Prefill the init name with the folder name when left blank.
    if (browserTarget.id === 'init-root') {
      const nameInput = document.getElementById('init-name');
      if (!nameInput.value.trim()) nameInput.value = basename(browserDir);
    }
  }
  closeBrowser();
}

async function refresh() {
  let repos;
  try {
    repos = (await daemon.call('GET', '/repos')) ?? [];
  } catch (error) {
    await statusBar.error(error);
    return;
  }
  empty.hidden = repos.length > 0;
  list.replaceChildren(
    ...repos.map((repo) =>
      el(
        'li',
        { onclick: () => openRepo(repo.repo_uuid) },
        el('strong', {}, repo.name),
        el('span', { class: 'root' }, repo.root),
        el('span', { class: 'uuid' }, repo.repo_uuid.slice(0, 8)),
      ),
    ),
  );
}

// Selecting a repo: adopt it in place when the workspace has none yet
// (startup case), otherwise open a new workspace (spec-gui "Repo
// indicator").
async function openRepo(repoUuid) {
  try {
    const current = await workspace.get('active_repo');
    if (current === null) {
      await workspace.adoptRepo(repoUuid);
      await commands.invoke('panel:set-type metarecord-list');
    } else {
      await commands.invoke(`tab:new ${repoUuid}`);
    }
  } catch (error) {
    statusBar.message(`cannot open the repository: ${error.message ?? error}`, 8000);
  }
}

async function submit(form, path, payload, errorElement) {
  errorElement.textContent = '';
  try {
    const created = await daemon.call('POST', path, payload);
    toggleForm(form, false);
    statusBar.message(`Repository ready: ${created.repo_uuid.slice(0, 8)}…`, 5000);
    await refresh();
    await commands.invoke(`tab:new ${created.repo_uuid}`);
  } catch (error) {
    errorElement.textContent = String(error.message ?? error);
  }
}

initForm.addEventListener('submit', (event) => {
  event.preventDefault();
  const root = document.getElementById('init-root').value.trim();
  const name = document.getElementById('init-name').value.trim();
  const payload = name ? { root, name } : { root };
  void submit(initForm, '/repos/init', payload, document.getElementById('init-error'));
});

loadForm.addEventListener('submit', (event) => {
  event.preventDefault();
  const root = document.getElementById('load-root').value.trim();
  void submit(loadForm, '/repos/load', { root }, document.getElementById('load-error'));
});

document.getElementById('show-init').addEventListener('click', () => toggleForm(initForm, true));
document.getElementById('show-load').addEventListener('click', () => toggleForm(loadForm, true));
document.getElementById('refresh').addEventListener('click', refresh);
for (const button of document.querySelectorAll('.cancel')) {
  button.addEventListener('click', () => toggleForm(button.closest('form'), false));
}
for (const button of document.querySelectorAll('.browse')) {
  button.addEventListener('click', () =>
    openBrowser(document.getElementById(button.dataset.target)),
  );
}
document.getElementById('browser-up').addEventListener('click', () => void browseTo(parentDir(browserDir)));
document.getElementById('browser-pick').addEventListener('click', pickFolder);
document.getElementById('browser-cancel').addEventListener('click', closeBrowser);

await metafolder.ready;
commands.register('repos:init', {
  label: 'Repos: open the init form',
  handler: () => toggleForm(initForm, true),
});
commands.register('repos:load', {
  label: 'Repos: open the load form',
  handler: () => toggleForm(loadForm, true),
});
commands.register('repos:refresh', {
  label: 'Repos: refresh the repository list',
  handler: refresh,
});
await refresh();
