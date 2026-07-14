// repos panel: list loaded repositories, init/load new ones, open a
// repository in a workspace (spec-gui "Repository management").

import { byId, el, qsa } from '/__ui.js';
import { createPickRunner } from '/__value-widget.js';

/**
 * A loaded repository, as `GET /repos` lists it.
 * @typedef {{repo_uuid: string, name: string, root: string}} Repo
 *
 * An in-flight daemon task (spec-tasks).
 * @typedef {{id: string, repo_uuid: string, kind: string, status: string,
 *            phase?: string|null, done: number|null, total: number|null}} Task
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { daemon, workspace, commands, statusBar, fs, messages } = metafolder;
  // Timing knobs (config.toml `[panels]`), with the former hard-coded fallbacks.
  const { settings } = metafolder;
  const statusErrorMs = settings.statusErrorMs ?? 8000;
  const taskPollMs = settings.taskPollMs ?? 1500;

  // Schema↔data inconsistencies are surfaced once, when a repo is opened (the
  // schema is read at repo load). Best-effort and capped: the daemon stops the
  // scan at the cap, so a repo with hundreds of thousands of violations stays
  // cheap, and we only show a heads-up — the schema takes priority either way.
  const SCHEMA_CHECK_CAP = 20;
  /** @type {Set<string>} */
  const announcedRepos = new Set();
  /** @param {string} repoUuid */
  async function announceSchemaConflicts(repoUuid) {
    if (announcedRepos.has(repoUuid)) return;
    announcedRepos.add(repoUuid);
    try {
      const res = /** @type {{violations?: unknown[], truncated?: boolean}} */ (
        await daemon.call('POST', `/repos/${repoUuid}/schema/check`, {
          limit: SCHEMA_CHECK_CAP,
        })
      );
      const n = res?.violations?.length ?? 0;
      if (n === 0) return;
      const count = res.truncated ? `${n}+` : `${n}`;
      const noun = n === 1 && !res.truncated ? 'inconsistency' : 'inconsistencies';
      void messages.append(
        `schema: ${count} ${noun} with existing data (schema takes priority; run a schema check for details)`,
      );
    } catch {
      // best-effort: a missing/!schema or transient error is not worth surfacing
    }
  }

  const list = byId(root, 'repo-list');
  const empty = byId(root, 'empty');
  const initForm = byId(root, 'init-form', HTMLFormElement);
  const loadForm = byId(root, 'load-form', HTMLFormElement);
  const retypeForm = byId(root, 'retype-form', HTMLFormElement);
  /** @type {string|null} repo uuid the retype form acts on */
  let retypeTarget = null;

  // ── Folder picker ─────────────────────────────────────────────────────────
  // Reuses the value-picker system (spec-gui "Value picker"): "Browse…" opens
  // the file-manager in the other slot and returns the chosen folder path.
  const pickRunner = createPickRunner(metafolder);
  /** @type {string|null} cached; the default start when the input is empty */
  let homeDir = null;

  /** @param {string} path */
  function basename(path) {
    return path.split('/').filter(Boolean).pop() ?? path;
  }

  /** @param {HTMLElement} form @param {boolean} show */
  function toggleForm(form, show) {
    form.classList.toggle('hidden', !show);
    if (show) form.querySelector('input')?.focus();
  }

  async function homeDirCached() {
    if (homeDir === null) {
      try {
        homeDir = await fs.homeDir();
      } catch {
        homeDir = '/';
      }
    }
    return homeDir;
  }

  /** @param {HTMLInputElement} targetInput */
  async function browseFolder(targetInput) {
    const start = targetInput.value.trim() || (await homeDirCached());
    const path = await pickRunner.request({
      panel: 'file-manager',
      vars: { 'file-manager:start-dir': start },
      result: 'path',
      repo: null, // browse the raw disk: the folder is not a repo yet
      name: 'Pick a folder',
      prompt:
        'Highlight a folder (“.” = current directory) — Ctrl+Enter to confirm, Ctrl+Esc to cancel',
    });
    if (!path) return; // cancelled
    targetInput.value = path;
    // Prefill the init name with the folder name when left blank.
    if (targetInput.id === 'init-root') {
      const nameInput = byId(root, 'init-name', HTMLInputElement);
      if (!nameInput.value.trim()) nameInput.value = basename(path);
    }
  }

  async function refresh() {
    /** @type {Repo[]} */
    let repos;
    try {
      repos = /** @type {Repo[]} */ ((await daemon.call('GET', '/repos')) ?? []);
    } catch (error) {
      await statusBar.error(error);
      return;
    }
    empty.hidden = repos.length > 0;
    list.replaceChildren(
      ...repos.map((repo) =>
        el(
          'li',
          { class: 'repo' },
          // Only the header opens the repo; the tasks block below it carries its
          // own (stop) buttons, so it must not share the row's click target.
          el(
            'div',
            { class: 'repo-head', onclick: () => openRepo(repo.repo_uuid) },
            el('strong', {}, repo.name),
            el('span', { class: 'root' }, repo.root),
            el('span', { class: 'uuid' }, repo.repo_uuid.slice(0, 8)),
            el(
              'button',
              {
                class: 'repo-unload',
                type: 'button',
                title: 'Convert a field type across this repository',
                onclick: (/** @type {Event} */ event) => {
                  event.stopPropagation();
                  openRetype(repo.repo_uuid, repo.name);
                },
              },
              'Retype…',
            ),
            el(
              'button',
              {
                class: 'repo-unload',
                type: 'button',
                title: 'Unload this repository from the daemon',
                // The header row opens the repo on click; keep that from firing.
                onclick: (/** @type {Event} */ event) => {
                  event.stopPropagation();
                  void unloadRepo(repo.repo_uuid);
                },
              },
              'Unload',
            ),
          ),
          el('ul', { class: 'repo-tasks', 'data-tasks-for': repo.repo_uuid }),
        ),
      ),
    );
    // Repaint the (now empty) task blocks right away so they don't wait a full
    // poll interval to appear.
    await pollTasks();
  }

  // ── Running tasks ─────────────────────────────────────────────────────────
  // Poll the daemon for in-flight tasks (spec-tasks) and surface the active
  // ones under their repository, each with a Stop button. Reconcile and query
  // are cancellable; flush is shown but not stoppable.
  const CANCELLABLE = new Set(['reconcile', 'query']);

  async function pollTasks() {
    /** @type {Task[]} */
    let tasks;
    try {
      tasks = /** @type {Task[]} */ ((await daemon.call('GET', '/tasks')) ?? []);
    } catch {
      return; // A transient daemon hiccup: leave the last paint in place.
    }
    /** @type {Map<string, Task[]>} */
    const byRepo = new Map();
    for (const task of tasks) {
      if (task.status !== 'running' && task.status !== 'pending') continue;
      const known = byRepo.get(task.repo_uuid);
      if (known) known.push(task);
      else byRepo.set(task.repo_uuid, [task]);
    }
    for (const container of qsa(list, '.repo-tasks')) {
      const forRepo = container.dataset.tasksFor;
      if (!forRepo) continue;
      renderTasks(container, forRepo, byRepo.get(forRepo) ?? []);
    }
  }

  /** @param {HTMLElement} container @param {string} repoUuid @param {Task[]} tasks */
  function renderTasks(container, repoUuid, tasks) {
    container.replaceChildren(
      ...tasks.map((task) => {
        const progress =
          task.done !== null && task.total !== null ? ` ${task.done}/${task.total}` : '';
        const label = `${task.kind}: ${task.phase || task.status}${progress}`;
        const children = [el('span', { class: 'task-label' }, label)];
        if (CANCELLABLE.has(task.kind)) {
          children.push(
            el(
              'button',
              { class: 'task-stop', type: 'button', onclick: () => void stopTask(repoUuid, task.id) },
              'Stop',
            ),
          );
        }
        return el('li', { class: 'repo-task' }, ...children);
      }),
    );
  }

  /** @param {string} repoUuid @param {string} taskId */
  async function stopTask(repoUuid, taskId) {
    try {
      await daemon.call('POST', `/repos/${repoUuid}/tasks/${taskId}/cancel`);
      void statusBar.message('stopping task…', 3000);
    } catch (error) {
      void statusBar.message(`cannot stop task: ${messageOf(error)}`, 6000);
    }
    await pollTasks();
  }

  // Unload a repository from the daemon (spec-main "Repository management"):
  // stops its watcher and releases its DB lock, then refreshes the list.
  /** @param {string} repoUuid */
  async function unloadRepo(repoUuid) {
    try {
      await daemon.call('POST', `/repos/${repoUuid}/unload`);
      void statusBar.message('repository unloaded', 3000);
    } catch (error) {
      void statusBar.message(`cannot unload: ${messageOf(error)}`, 6000);
    }
    await refresh();
  }

  // Selecting a repo: adopt it in place when the workspace has none yet
  // (startup case), otherwise open a new workspace.
  /** @param {string} repoUuid */
  async function openRepo(repoUuid) {
    try {
      const current = await workspace.get('active_repo');
      if (current === null) {
        await workspace.adoptRepo(repoUuid);
        await commands.invoke('panel:set-type metarecord-list');
      } else {
        await commands.invoke(`tab:new ${repoUuid}`);
      }
      void announceSchemaConflicts(repoUuid); // once-per-repo heads-up
    } catch (error) {
      void statusBar.message(`cannot open the repository: ${messageOf(error)}`, statusErrorMs);
    }
  }

  /**
   * @param {HTMLElement} form @param {string} path @param {unknown} payload
   * @param {HTMLElement} errorElement
   */
  async function submit(form, path, payload, errorElement) {
    errorElement.textContent = '';
    try {
      const created = /** @type {{repo_uuid: string}} */ (
        await daemon.call('POST', path, payload)
      );
      toggleForm(form, false);
      await refresh();
      const current = await workspace.get('active_repo');
      if (current === null) {
        await workspace.adoptRepo(created.repo_uuid);
        await commands.invoke('panel:set-type metarecord-list');
      } else {
        void statusBar.message(
          `Repository ready: ${created.repo_uuid.slice(0, 8)}… (open it from the list)`,
          6000,
        );
      }
    } catch (error) {
      errorElement.textContent = messageOf(error);
    }
  }

  // ── Retype a field across a whole repository (spec-data-model) ─────────────

  /** @param {string} repoUuid @param {string} repoName */
  function openRetype(repoUuid, repoName) {
    retypeTarget = repoUuid;
    byId(root, 'retype-target').textContent = `Repository: ${repoName}`;
    byId(root, 'retype-error').textContent = '';
    byId(root, 'retype-name', HTMLInputElement).value = '';
    toggleForm(retypeForm, true);
  }

  /** @param {Event} event */
  async function onRetypeSubmit(event) {
    event.preventDefault();
    const errorElement = byId(root, 'retype-error');
    errorElement.textContent = '';
    const name = byId(root, 'retype-name', HTMLInputElement).value.trim();
    const to = byId(root, 'retype-type', HTMLSelectElement).value;
    if (!name || !retypeTarget) return;
    try {
      // Count the metarecords carrying the field, to describe the change.
      const count = /** @type {{total?: number|null}} */ (
        await daemon.call('POST', `/repos/${retypeTarget}/query`, {
          query: { type: 'is_present', field: name },
          select: '*',
          limit: 1,
          count: true,
        })
      );
      const n = count.total ?? 0;
      const ok = confirm(
        `Convert field "${name}" to ${to} on ${n} metarecord${n === 1 ? '' : 's'} ` +
          `across this repository?\n\nValues that cannot be converted fall back to ` +
          `the type's default (and Nothing rows are left untouched).`,
      );
      if (!ok) return;
      const resp = /** @type {{converted?: number, fallback_count?: number}} */ (
        await daemon.call('POST', `/repos/${retypeTarget}/retype`, { name, to })
      );
      toggleForm(retypeForm, false);
      const converted = resp.converted ?? 0;
      const fell = resp.fallback_count ?? 0;
      void statusBar.message(
        `Retyped "${name}" to ${to}: ${converted} value(s) converted` +
          (fell > 0 ? `, ${fell} fell back to the default` : '') + '.',
        7000,
      );
      // Other panels reading this repo should refresh.
      await workspace.set('metarecords:dirty', Date.now());
    } catch (error) {
      errorElement.textContent = messageOf(error);
    }
  }
  retypeForm.addEventListener('submit', (event) => void onRetypeSubmit(event));

  initForm.addEventListener('submit', (event) => {
    event.preventDefault();
    const root_ = byId(root, 'init-root', HTMLInputElement).value.trim();
    const name = byId(root, 'init-name', HTMLInputElement).value.trim();
    const payload = name ? { root: root_, name } : { root: root_ };
    void submit(initForm, '/repos/init', payload, byId(root, 'init-error'));
  });

  loadForm.addEventListener('submit', (event) => {
    event.preventDefault();
    const root_ = byId(root, 'load-root', HTMLInputElement).value.trim();
    void submit(loadForm, '/repos/load', { root: root_ }, byId(root, 'load-error'));
  });

  byId(root, 'show-init').addEventListener('click', () => toggleForm(initForm, true));
  byId(root, 'show-load').addEventListener('click', () => toggleForm(loadForm, true));
  byId(root, 'refresh').addEventListener('click', () => void refresh());
  for (const button of qsa(root, '.cancel')) {
    button.addEventListener('click', () => {
      const form = button.closest('form');
      if (form) toggleForm(form, false);
    });
  }
  for (const button of qsa(root, '.browse')) {
    button.addEventListener('click', () => {
      const target = button.dataset.target;
      if (target) void browseFolder(byId(root, target, HTMLInputElement));
    });
  }

  void commands.register('repos:init', {
    label: 'Repos: open the init form',
    handler: () => toggleForm(initForm, true),
  });
  void commands.register('repos:load', {
    label: 'Repos: open the load form',
    handler: () => toggleForm(loadForm, true),
  });
  void commands.register('repos:refresh', {
    label: 'Repos: refresh the repository list',
    handler: () => refresh(),
  });
  void commands.register('repos:retype', {
    label: 'Repos: convert a field type across the active repository',
    reveal: true,
    handler: async () => {
      const repoUuid = /** @type {string|null} */ ((await workspace.get('active_repo')) ?? null);
      if (!repoUuid) {
        void statusBar.message('no active repository', 4000);
        return;
      }
      const repos = /** @type {Repo[]} */ ((await daemon.call('GET', '/repos')) ?? []);
      const repo = repos.find((r) => r.repo_uuid === repoUuid);
      openRetype(repoUuid, repo?.name ?? repoUuid.slice(0, 8));
    },
  });
  await refresh();

  // Keep the per-repo task blocks live while the panel is mounted.
  const taskTimer = setInterval(() => void pollTasks(), taskPollMs);
  return () => clearInterval(taskTimer);
}

/** The message of a thrown daemon error (`{"error": …}` bodies arrive as Error). */
function messageOf(/** @type {unknown} */ error) {
  return error instanceof Error ? error.message : String(error);
}
