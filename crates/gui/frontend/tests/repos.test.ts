// repos panel: the first screen of a fresh install — list, init, load, open,
// unload — driven through the *real* `createPanelApi` against a fake daemon.
//
// Every other panel test hands the panel a hand-written stub API. That checks
// the panel against a copy of the surface, not against the surface: a method a
// panel calls that `createPanelApi` does not actually provide passes the test
// and throws in the real GUI. Here the panel gets the real API object, with
// only the Tauri `invoke` seam replaced by a router over an in-memory daemon.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { createPanelApi } from '../src/lib/panels/api';
import { sharedCache } from '../src/lib/panels/api';
import { mount } from '../../default-config/panel-types/repos/main.js';

/** The shell's mount path: index.html body (minus scripts/styles) in a Shadow root. */
function shadowForRepos(): ShadowRoot {
  const html = readFileSync(
    resolve(process.cwd(), '../default-config/panel-types/repos/index.html'),
    'utf8',
  );
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const host = document.createElement('div');
  document.body.append(host);
  const shadow = host.attachShadow({ mode: 'open' });
  const body = document.createElement('div');
  body.className = 'mf-panel-body';
  for (const child of [...doc.body.childNodes]) {
    if (child.nodeName === 'SCRIPT' || child.nodeName === 'STYLE') continue;
    body.append(child);
  }
  shadow.append(body);
  return shadow;
}

interface Repo {
  repo_uuid: string;
  name: string;
  root: string;
  internal_dir?: string;
}

/** A minimal in-memory stand-in for the daemon's repository endpoints. */
function fakeDaemon() {
  const repos: Repo[] = [];
  const tasks: Record<string, unknown>[] = [];
  const cancelled: string[] = [];
  // Ingestion state of every repo of this fake daemon (spec-file-tracking
  // "Watch status, pause and resume").
  const watch = { paused: false, pending: 0 };
  let nextUuid = 1;
  const uuid = () => String(nextUuid++).padStart(32, '0');
  const calls: { method: string; path: string; body: unknown }[] = [];

  function request(method: string, path: string, body: unknown) {
    calls.push({ method, path, body });
    const bare = path.split('?')[0];
    if (method === 'GET' && bare === '/repos') return { status: 200, body: repos.slice() };
    if (method === 'GET' && bare === '/tasks') return { status: 200, body: tasks.slice() };
    if (method === 'POST' && /^\/repos\/[^/]+\/tasks\/[^/]+\/cancel$/.test(bare)) {
      cancelled.push(bare.split('/')[4]);
      return { status: 200, body: {} };
    }
    if (method === 'POST' && /^\/repos\/[^/]+\/query$/.test(bare)) {
      // The retype form counts the records carrying the field first.
      return { status: 200, body: { results: [], total: 3, next_cursor: null } };
    }
    if (method === 'POST' && /^\/repos\/[^/]+\/retype$/.test(bare)) {
      const b = body as { name: string; to: string };
      return { status: 200, body: { converted: 3, fallback_count: 1, name: b.name, to: b.to } };
    }
    if (method === 'POST' && bare === '/repos/init') {
      const b = body as { root: string; name?: string };
      const repo = {
        repo_uuid: uuid(),
        name: b.name ?? (b.root.split('/').filter(Boolean).pop() ?? 'repo'),
        root: b.root,
        internal_dir: `${b.root}/.metafolder/internal`,
      };
      repos.push(repo);
      return { status: 200, body: repo };
    }
    if (method === 'POST' && bare === '/repos/load') {
      const b = body as { root: string };
      const known = repos.find((r) => r.root === b.root);
      if (known) return { status: 200, body: known };
      return { status: 400, body: { error: `not a repository: ${b.root}` } };
    }
    if (method === 'POST' && /^\/repos\/[^/]+\/unload$/.test(bare)) {
      const id = bare.split('/')[2];
      const at = repos.findIndex((r) => r.repo_uuid === id);
      if (at < 0) return { status: 404, body: { error: 'Repository not found' } };
      repos.splice(at, 1);
      return { status: 200, body: {} };
    }
    if (method === 'POST' && /^\/repos\/[^/]+\/schema\/check$/.test(bare)) {
      return { status: 200, body: { violations: [] } };
    }
    if (method === 'GET' && /^\/repos\/[^/]+\/watch$/.test(bare)) {
      return { status: 200, body: { paused: watch.paused, pending_events: watch.pending } };
    }
    if (method === 'POST' && /^\/repos\/[^/]+\/watch\/resume$/.test(bare)) {
      watch.paused = false;
      return { status: 200, body: { paused: false, pending_events: watch.pending } };
    }
    return { status: 404, body: { error: `unrouted ${method} ${bare}` } };
  }

  return { repos, tasks, cancelled, watch, request, calls };
}

/** The real panel API, with `invoke` routed to the fake daemon. */
function setup(options: { activeRepo?: string | null } = {}) {
  const daemon = fakeDaemon();
  const vars = new Map<string, unknown>([['active_repo', options.activeRepo ?? null]]);
  const dispatch = vi.fn(async (_invocation: string) => {});
  const handlers = new Map<string, (...a: string[]) => unknown>();
  // `repo_init` is a Tauri command, not an HTTP call: it does POST /repos/init
  // plus the ignore preset, and returns the new uuid (crates/gui/src/repo_init.rs).
  const initRepo = vi.fn(async (args: Record<string, unknown>) => {
    const res = daemon.request('POST', '/repos/init', { root: args.root, name: args.name });
    if (res.status >= 400) throw new Error(String((res.body as { error?: string }).error));
    return (res.body as Repo).repo_uuid;
  });

  const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case 'daemon_request':
        return daemon.request(
          args!.method as string,
          args!.path as string,
          args!.body ?? null,
        );
      case 'repo_init':
        return initRepo(args!);
      case 'ws_get_var':
        return vars.get(args!.key as string) ?? null;
      case 'ws_set_var':
        vars.set(args!.key as string, args!.value);
        return null;
      case 'adopt_repo':
        vars.set('active_repo', args!.repo);
        return null;
      case 'register_command':
        return null;
      case 'post_status':
      case 'append_message':
      case 'suggest_keybinding':
      case 'bench_record':
      case 'picker_seed':
      case 'fs_home_dir':
        return command === 'fs_home_dir' ? '/home/user' : null;
      default:
        throw new Error(`unexpected Tauri command: ${command}`);
    }
  });

  const instance = createPanelApi(
    {
      invoke,
      dispatch,
      registerHandler: (name, handler) => handlers.set(name, handler),
      onCommandsChanged: () => {},
      addDefaultMenuItems: () => {},
    },
    {
      wsId: 'ws-1',
      panelType: 'repos',
      guiServer: 'http://127.0.0.1:7524',
      sessionToken: 'test-token',
      root: {} as ShadowRoot,
      visibilityGate: { visible: true, set: () => {}, whenVisible: (fn: () => void) => fn() },
    },
  );
  return { api: instance.api, daemon, vars, dispatch, invoke, handlers, initRepo };
}

/** The panel's own commands, as the shell would invoke them. */
function command(handlers: Map<string, (...a: string[]) => unknown>, name: string) {
  const handler = handlers.get(name);
  if (!handler) throw new Error(`the panel never registered ${name}`);
  return handler;
}

/** Lets the panel's chained awaits settle (mount → refresh → pollTasks). */
const settle = async () => {
  for (let i = 0; i < 8; i++) await Promise.resolve();
  await new Promise((r) => setTimeout(r, 0));
};

/** Opens the form the way the user does (the buttons above the list). */
function openForm(shadow: ShadowRoot, buttonId: string) {
  (shadow.getElementById(buttonId) as HTMLElement).click();
}

function submitForm(shadow: ShadowRoot, id: string, values: Record<string, string>) {
  for (const [inputId, value] of Object.entries(values)) {
    (shadow.getElementById(inputId) as HTMLInputElement).value = value;
  }
  const form = shadow.getElementById(id) as HTMLFormElement;
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
}

describe('repos panel', () => {
  beforeEach(() => {
    // A fresh realm per test: the cache is a module-level singleton shared by
    // every panel, so a repo listed in one test must not leak into the next.
    for (const repo of sharedCache.trackedRepos()) sharedCache.clearRepo(repo);
    vi.useRealTimers();
  });

  test('an empty daemon shows the empty notice and no rows', async () => {
    const { api } = setup();
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(0);
    expect((shadow.getElementById('empty') as HTMLElement).hidden).toBe(false);
  });

  test('init creates the repository, lists it and adopts it in the workspace', async () => {
    const { api, daemon, vars, dispatch, initRepo } = setup({ activeRepo: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    openForm(shadow, 'show-init');
    expect((shadow.getElementById('init-form') as HTMLElement).classList.contains('hidden')).toBe(
      false,
    );
    submitForm(shadow, 'init-form', { 'init-root': '/tmp/photos', 'init-name': 'photos' });
    await settle();

    // The creation went through the Tauri command (which also applies the
    // default ignore preset), never a bare POST /repos/init from the panel.
    expect(initRepo).toHaveBeenCalledWith(expect.objectContaining({ root: '/tmp/photos' }));
    expect(daemon.repos).toHaveLength(1);
    // The list repainted, the form closed, no error text.
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(1);
    expect((shadow.getElementById('init-form') as HTMLElement).classList.contains('hidden')).toBe(
      true,
    );
    expect((shadow.getElementById('init-error') as HTMLElement).textContent).toBe('');
    // A workspace with no repository adopts the new one and opens the list.
    expect(vars.get('active_repo')).toBe(daemon.repos[0].repo_uuid);
    expect(dispatch).toHaveBeenCalledWith('panel:set-type metarecord-list');
  });

  test('a failed load shows the daemon error in the form, not a blank panel', async () => {
    const { api, daemon } = setup();
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    openForm(shadow, 'show-load');
    submitForm(shadow, 'load-form', { 'load-root': '/tmp/not-a-repo' });
    await settle();

    expect((shadow.getElementById('load-error') as HTMLElement).textContent).toContain(
      'not a repository',
    );
    // The form stays open so the user can correct the path.
    expect((shadow.getElementById('load-form') as HTMLElement).classList.contains('hidden')).toBe(
      false,
    );
    expect(daemon.repos).toHaveLength(0);
  });

  test('opening a repository when one is already active opens a new tab', async () => {
    const { api, daemon, dispatch, vars } = setup({ activeRepo: 'other-repo' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    (shadow.querySelector('#repo-list .repo-head') as HTMLElement).click();
    await settle();

    expect(dispatch).toHaveBeenCalledWith('workspace:new r1');
    expect(vars.get('active_repo')).toBe('other-repo'); // unchanged
  });

  test('unload drops the repository from the list', async () => {
    const { api, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(1);

    (shadow.querySelector('#repo-list .repo-unload:last-of-type') as HTMLElement).click();
    await settle();

    expect(daemon.repos).toHaveLength(0);
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(0);
  });
});

describe('repos panel — running tasks', () => {
  test('an in-flight task is shown under its repository and can be stopped', async () => {
    const { api, daemon, handlers } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.tasks.push({
      id: 't1',
      repo_uuid: 'r1',
      kind: 'reconcile',
      status: 'running',
      phase: 'scanning',
      done: 12,
      total: 40,
    });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const task = shadow.querySelector('.repo-tasks .repo-task') as HTMLElement;
    expect(task?.textContent).toContain('reconcile');
    expect(task?.textContent).toContain('12/40');

    (task.querySelector('.task-stop') as HTMLElement).click();
    await settle();
    expect(daemon.cancelled).toEqual(['t1']);

    // A finished task is no longer listed.
    daemon.tasks.length = 0;
    await command(handlers, 'repos:refresh')();
    await settle();
    expect(shadow.querySelectorAll('.repo-tasks .repo-task')).toHaveLength(0);
  });

  test('a task that cannot be cancelled gets no Stop button', async () => {
    const { api, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.tasks.push({
      id: 't2',
      repo_uuid: 'r1',
      kind: 'load',
      status: 'running',
      phase: null,
      done: null,
      total: null,
    });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const task = shadow.querySelector('.repo-tasks .repo-task') as HTMLElement;
    expect(task?.textContent).toContain('load');
    expect(task.querySelector('.task-stop')).toBeNull();
  });

  test('a flush can be stopped — it is the one internal task the user sizes', async () => {
    const { api, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.tasks.push({
      id: 't3',
      repo_uuid: 'r1',
      kind: 'flush',
      status: 'running',
      phase: 'flush',
      done: null,
      total: null,
    });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const task = shadow.querySelector('.repo-tasks .repo-task') as HTMLElement;
    expect(task?.textContent).toContain('flush');
    (task.querySelector('.task-stop') as HTMLElement).click();
    await settle();
    expect(daemon.cancelled).toEqual(['t3']);
  });
});

describe('repos panel — paused tracking', () => {
  test('a paused repository says so, and Resume restarts it', async () => {
    const { api, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.watch.paused = true;
    daemon.watch.pending = 1204;

    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const notice = shadow.querySelector('.repo-watch') as HTMLElement;
    expect(notice.textContent).toContain('tracking paused');
    expect(notice.textContent).toContain('1204');

    (notice.querySelector('.watch-resume') as HTMLElement).click();
    await settle();
    expect(daemon.watch.paused).toBe(false);

    // Once resumed, the notice goes away.
    await settle();
    expect((shadow.querySelector('.repo-watch') as HTMLElement).textContent).toBe('');
  });
});

describe('repos panel — field retype', () => {
  test('the command opens the form for the active repository and converts', async () => {
    const { api, daemon, handlers, vars } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await command(handlers, 'repos:open-retype')();
    await settle();
    const form = shadow.getElementById('retype-form') as HTMLElement;
    expect(form.classList.contains('hidden')).toBe(false);
    expect((shadow.getElementById('retype-target') as HTMLElement).textContent).toContain('photos');

    vi.stubGlobal('confirm', vi.fn(() => true));
    submitForm(shadow, 'retype-form', { 'retype-name': 'rating' });
    await settle();

    const retype = daemon.calls.find((c) => c.path.endsWith('/retype'));
    expect(retype?.body).toEqual({ name: 'rating', to: 'string' });
    expect(form.classList.contains('hidden')).toBe(true);
    // Other panels are told the data changed.
    expect(vars.get('metarecords:dirty')).toEqual(expect.any(Number));
  });

  test('declining the confirmation converts nothing', async () => {
    const { api, daemon, handlers } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    await command(handlers, 'repos:open-retype')();
    await settle();

    vi.stubGlobal('confirm', vi.fn(() => false));
    submitForm(shadow, 'retype-form', { 'retype-name': 'rating' });
    await settle();

    expect(daemon.calls.some((c) => c.path.endsWith('/retype'))).toBe(false);
  });

  test('with no active repository the command says so instead of opening', async () => {
    const { api, handlers } = setup({ activeRepo: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await command(handlers, 'repos:open-retype')();
    await settle();
    expect((shadow.getElementById('retype-form') as HTMLElement).classList.contains('hidden')).toBe(
      true,
    );
  });
});

describe('repos panel — commands', () => {
  test('open-init and open-load reveal their forms', async () => {
    const { api, handlers } = setup();
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await command(handlers, 'repos:open-init')();
    expect((shadow.getElementById('init-form') as HTMLElement).classList.contains('hidden')).toBe(
      false,
    );
    await command(handlers, 'repos:open-load')();
    expect((shadow.getElementById('load-form') as HTMLElement).classList.contains('hidden')).toBe(
      false,
    );
    // Cancel closes the form again.
    (shadow.querySelector('#load-form .cancel') as HTMLElement).click();
    expect((shadow.getElementById('load-form') as HTMLElement).classList.contains('hidden')).toBe(
      true,
    );
  });

  test('refresh picks up a repository loaded from elsewhere', async () => {
    const { api, daemon, handlers } = setup();
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(0);

    daemon.repos.push({ repo_uuid: 'r9', name: 'later', root: '/tmp/later' });
    await command(handlers, 'repos:refresh')();
    await settle();
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(1);
  });
});
