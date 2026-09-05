// repos panel: list loaded repositories, init/load new ones, open a
// repository in a workspace (spec-gui "Repository management").

import { byId, el, qsa } from '/__ui.js';
import { createPickRunner } from '/__value-widget.js';
import { createSelect } from '/__select.js';

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
  const retypeType = createSelect(byId(root, 'retype-type'), {
    value: 'string',
    options: [
      'string',
      'int',
      'float',
      'bool',
      'datetime',
      'ref',
      'tree_ref',
      'externalref',
      'refbase',
    ].map((t) => ({ value: t })),
  });
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
          el('div', { class: 'repo-watch', 'data-watch-for': repo.repo_uuid }),
        ),
      ),
    );
    // Repaint the (now empty) task blocks right away so they don't wait a full
    // poll interval to appear.
    await pollTasks();
    await pollWatch();
  }

  // ── Running tasks ─────────────────────────────────────────────────────────
  // Poll the daemon for in-flight tasks (spec-tasks) and surface the active
  // ones under their repository, each with a Stop button. Reconcile, query and
  // flush are cancellable — stopping a flush pauses the repository's tracking
  // (spec-file-tracking "Pausing ingestion"), which the row below then offers
  // to resume.
  const CANCELLABLE = new Set(['reconcile', 'query', 'flush']);

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
              {
                class: 'task-stop',
                type: 'button',
                onclick: () => void stopTask(repoUuid, task.id, task.kind),
              },
              'Stop',
            ),
          );
        }
        return el('li', { class: 'repo-task' }, ...children);
      }),
    );
  }

  /** @param {string} repoUuid @param {string} taskId @param {string} kind */
  async function stopTask(repoUuid, taskId, kind) {
    try {
      await daemon.call('POST', `/repos/${repoUuid}/tasks/${taskId}/cancel`);
      // A stopped flush leaves the repository paused on purpose: say so, or the
      // user is left wondering why nothing is being tracked any more.
      void statusBar.message(
        kind === 'flush' ? 'flush stopped — tracking paused, nothing lost' : 'stopping task…',
        kind === 'flush' ? 6000 : 3000,
      );
    } catch (error) {
      void statusBar.message(`cannot stop task: ${messageOf(error)}`, 6000);
    }
    await pollTasks();
    await pollWatch();
  }

  // ── Tracking notices ──────────────────────────────────────────────────────
  // Three conditions leave a repository recording less than it looks like it
  // does, and each one makes it look broken if it is not said out loud: its
  // ingestion is paused, its watch budget could not cover the whole tree, or
  // the kernel is refusing watches to a daemon that has not reached its own
  // ceiling. All three live on the repository's row.

  /**
   * @typedef {{limit: number|null, share: number, cap: number|null,
   *            starved: boolean, exceeded_dirs: number}} WatchBudget
   * @typedef {{paused?: boolean, pending_events?: number|null,
   *            watched_dirs?: number, watch_budget?: WatchBudget}} WatchStatus
   */

  async function pollWatch() {
    for (const container of qsa(list, '.repo-watch')) {
      const repoUuid = container.dataset.watchFor;
      if (!repoUuid) continue;
      /** @type {WatchStatus|null} */
      let status = null;
      try {
        status = /** @type {WatchStatus} */ (
          await daemon.call('GET', `/repos/${repoUuid}/watch`)
        );
      } catch {
        continue; // A transient hiccup: leave the last paint in place.
      }
      container.replaceChildren(...watchNotices(repoUuid, status));
    }
  }

  /**
   * The notices a repository's tracking state deserves, in order of how much
   * they mean: nothing at all when everything is watched and running.
   * @param {string} repoUuid
   * @param {WatchStatus|null} status
   * @returns {Element[]}
   */
  function watchNotices(repoUuid, status) {
    /** @type {Element[]} */
    const notices = [];
    if (!status) return notices;

    if (status.paused) {
      const waiting =
        typeof status.pending_events === 'number'
          ? ` — ${status.pending_events} event(s) waiting`
          : '';
      notices.push(
        el('span', { class: 'watch-paused' }, `tracking paused${waiting}`),
        el(
          'button',
          {
            class: 'watch-resume',
            type: 'button',
            title: 'Resume tracking and apply the buffered events',
            onclick: (/** @type {Event} */ event) => {
              event.stopPropagation();
              void resumeWatch(repoUuid);
            },
          },
          'Resume',
        ),
      );
    }

    const budget = status.watch_budget;
    if (!budget) return notices;

    // Someone else holds the watches. Transient and external, so the daemon
    // records nothing — which is exactly why it has to be visible for as long
    // as it lasts (spec-file-tracking "Two different failures").
    if (budget.starved) {
      notices.push(
        el(
          'span',
          {
            class: 'watch-starved',
            title:
              'Another program holds the inotify watches. This daemon is below its own ' +
              'ceiling, so nothing was recorded — changes in the directories it could not ' +
              'watch go unnoticed until a reconcile.',
          },
          'out of inotify watches',
        ),
      );
    }

    // Subtrees the budget could not afford. A count means little without the
    // names, so the notice opens them.
    if (budget.exceeded_dirs > 0) {
      const used = status.watched_dirs ?? 0;
      const of = budget.cap === null ? '' : ` of ${budget.cap}`;
      notices.push(
        el(
          'button',
          {
            class: 'watch-excluded',
            type: 'button',
            title:
              `${used}${of} watch(es) used (share ${budget.share}%). ` +
              'These subtrees are still tracked — reconcile walks them — but changes ' +
              'in them are not noticed live. Click to list them.',
            onclick: (/** @type {Event} */ event) => {
              event.stopPropagation();
              void showExceeded(repoUuid);
            },
          },
          `${budget.exceeded_dirs} subtree(s) unwatched`,
        ),
      );
    }
    return notices;
  }

  /**
   * Lists the subtrees left unwatched for want of budget.
   * @param {string} repoUuid
   */
  async function showExceeded(repoUuid) {
    try {
      const body = /** @type {{exceeded?: string[]}} */ (
        await daemon.call('GET', `/repos/${repoUuid}/watch/exceeded`)
      );
      const paths = body?.exceeded ?? [];
      void messages.append(
        paths.length
          ? `unwatched subtrees (watch budget): ${paths.join(', ')}`
          : 'no subtree is excluded',
      );
      void statusBar.message(`${paths.length} subtree(s) listed in the messages`, 4000);
    } catch (error) {
      void statusBar.message(`cannot list unwatched subtrees: ${messageOf(error)}`, 6000);
    }
  }

  /** @param {string} repoUuid */
  async function resumeWatch(repoUuid) {
    try {
      await daemon.call('POST', `/repos/${repoUuid}/watch/resume`);
      void statusBar.message('tracking resumed', 3000);
    } catch (error) {
      void statusBar.message(`cannot resume tracking: ${messageOf(error)}`, 6000);
    }
    await pollWatch();
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
        await commands.invoke(`workspace:new ${repoUuid}`);
      }
      void announceSchemaConflicts(repoUuid); // once-per-repo heads-up
    } catch (error) {
      void statusBar.error(`cannot open the repository: ${messageOf(error)}`, statusErrorMs);
    }
  }

  // Shared post-creation flow: hide the form, refresh the list, and either adopt
  // the new repo (if none is active) or announce it.
  /** @param {HTMLElement} form @param {string} repoUuid */
  async function onCreated(form, repoUuid) {
    toggleForm(form, false);
    await refresh();
    const current = await workspace.get('active_repo');
    if (current === null) {
      await workspace.adoptRepo(repoUuid);
      await commands.invoke('panel:set-type metarecord-list');
    } else {
      void statusBar.message(
        `Repository ready: ${repoUuid.slice(0, 8)}… (open it from the list)`,
        6000,
      );
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
      await onCreated(form, created.repo_uuid);
    } catch (error) {
      errorElement.textContent = messageOf(error);
    }
  }

  // Repo creation goes through daemon.initRepo (core::repo_init): it also applies
  // the `default` ignore preset to the new root, which a raw POST /repos/init
  // would skip (the daemon writes no default ignores itself).
  /**
   * @param {HTMLElement} form @param {{root: string, name?: string}} opts
   * @param {HTMLElement} errorElement
   */
  async function submitInit(form, opts, errorElement) {
    errorElement.textContent = '';
    try {
      const repoUuid = await daemon.initRepo(opts);
      await onCreated(form, repoUuid);
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
    const to = retypeType.get() ?? 'string';
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
    const opts = name ? { root: root_, name } : { root: root_ };
    void submitInit(initForm, opts, byId(root, 'init-error'));
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

  void commands.register('repos:open-init', {
    label: 'Repos: open the init form',
    handler: () => toggleForm(initForm, true),
  });
  void commands.register('repos:open-load', {
    label: 'Repos: open the load form',
    handler: () => toggleForm(loadForm, true),
  });
  void commands.register('repos:refresh', {
    label: 'Repos: refresh the repository list',
    handler: () => refresh(),
  });
  void commands.register('repos:open-retype', {
    label: 'Repos: open the field-type conversion form for the active repository',
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

  // Keep the per-repo task blocks — and the paused-tracking notice, which a
  // stop from anywhere else also raises — live while the panel is mounted.
  const taskTimer = setInterval(() => {
    void pollTasks();
    void pollWatch();
  }, taskPollMs);
  return () => clearInterval(taskTimer);
}

/** The message of a thrown daemon error (`{"error": …}` bodies arrive as Error). */
function messageOf(/** @type {unknown} */ error) {
  return error instanceof Error ? error.message : String(error);
}
