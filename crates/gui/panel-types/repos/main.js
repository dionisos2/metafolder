// repos panel: list loaded repositories, init/load new ones, open a
// repository in a workspace (spec-gui "Repository management").

import { el } from '/__ui.js';

const { daemon, workspace, commands, statusBar } = metafolder;

const list = document.getElementById('repo-list');
const empty = document.getElementById('empty');
const initForm = document.getElementById('init-form');
const loadForm = document.getElementById('load-form');

function toggleForm(form, show) {
  form.classList.toggle('hidden', !show);
  if (show) form.querySelector('input').focus();
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
      await commands.invoke('panel:set-type entry-list');
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
  void submit(initForm, '/repos/init', { root }, document.getElementById('init-error'));
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
