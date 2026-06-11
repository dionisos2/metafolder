// repos panel: list loaded repositories, init/load new ones, open a
// repository in a workspace (spec-gui "Repository management").

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
  const response = await daemon.request('GET', '/repos');
  if (response.status !== 200) {
    statusBar.message(response.body?.error ?? 'cannot list repositories');
    return;
  }
  const repos = response.body ?? [];
  empty.hidden = repos.length > 0;
  list.replaceChildren(
    ...repos.map((repo) => {
      const item = document.createElement('li');
      const name = document.createElement('strong');
      name.textContent = repo.name;
      const root = document.createElement('span');
      root.className = 'root';
      root.textContent = repo.root;
      const uuid = document.createElement('span');
      uuid.className = 'uuid';
      uuid.textContent = repo.repo_uuid.slice(0, 8);
      item.append(name, root, uuid);
      item.addEventListener('click', () => openRepo(repo.repo_uuid));
      return item;
    }),
  );
}

// Selecting a repo: adopt it in place when the workspace has none yet
// (startup case), otherwise open a new workspace (spec-gui "Repo
// indicator").
async function openRepo(repoUuid) {
  const current = await workspace.get('active_repo');
  if (current === null) {
    await workspace.adoptRepo(repoUuid);
    await commands.invoke('panel:set-type entry-list');
  } else {
    await commands.invoke(`tab:new ${repoUuid}`);
  }
}

async function submit(form, path, payload, errorElement) {
  errorElement.textContent = '';
  const response = await daemon.request('POST', path, payload);
  if (response.status === 200) {
    toggleForm(form, false);
    statusBar.message(`Repository ready: ${response.body.repo_uuid.slice(0, 8)}…`, 5000);
    await refresh();
    await commands.invoke(`tab:new ${response.body.repo_uuid}`);
  } else {
    errorElement.textContent = response.body?.error ?? `error ${response.status}`;
  }
}

initForm.addEventListener('submit', (event) => {
  event.preventDefault();
  const root = document.getElementById('init-root').value.trim();
  submit(initForm, '/repos/init', { root }, document.getElementById('init-error'));
});

loadForm.addEventListener('submit', (event) => {
  event.preventDefault();
  const root = document.getElementById('load-root').value.trim();
  submit(loadForm, '/repos/load', { root }, document.getElementById('load-error'));
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
