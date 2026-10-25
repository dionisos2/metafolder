// repos panel: the first screen of a fresh install — list, init, load, open,
// unload — driven through the *real* `createPanelApi` against a fake daemon.
//
// Every other panel test hands the panel a hand-written stub API. That checks
// the panel against a copy of the surface, not against the surface: a method a
// panel calls that `createPanelApi` does not actually provide passes the test
// and throws in the real GUI. Here the panel gets the real API object, with
// only the Tauri `invoke` seam replaced by a router over an in-memory daemon.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { generic } from './support/cursor';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { createPanelApi } from '../src/lib/panels/api';
import { changeFeed } from '../src/lib/panels/api';
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
  lost?: boolean;
  ready?: boolean;
  in_memory?: { bytes: number; pinned: boolean; reason: string | null } | null;
}

/** A minimal in-memory stand-in for the daemon's repository endpoints. */
function fakeDaemon() {
  const repos: Repo[] = [];
  const tasks: Record<string, unknown>[] = [];
  const cancelled: string[] = [];
  // Ingestion state of every repo of this fake daemon (doc "Watcher endpoints").
  const watch: {
    paused: boolean;
    pending: number;
    extra: Record<string, unknown>;
  } = { paused: false, pending: 0, extra: {} };
  let nextUuid = 1;
  const uuid = () => String(nextUuid++).padStart(32, '0');
  const calls: { method: string; path: string; body: unknown }[] = [];

  function request(method: string, path: string, body: unknown) {
    calls.push({ method, path, body });
    const bare = path.split('?')[0];
    if (method === 'GET' && bare === '/repos') {
      // Lost repositories only when asked for (doc "When the root moves").
      const lost = path.includes('lost=true');
      return { status: 200, body: repos.filter((r) => lost || !r.lost) };
    }
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
    if (method === 'POST' && /^\/repos\/[^/]+\/memory$/.test(bare)) {
      const repo = repos.find((r) => r.repo_uuid === bare.split('/')[2]);
      if (!repo) return { status: 404, body: { error: 'Repository not found' } };
      repo.in_memory = (body as { in_memory: boolean }).in_memory
        ? { bytes: 2 << 20, pinned: true, reason: null }
        : null;
      return { status: 200, body: repo };
    }
    if (method === 'POST' && /^\/repos\/[^/]+\/schema\/check$/.test(bare)) {
      return { status: 200, body: { violations: [] } };
    }
    if (method === 'GET' && /^\/repos\/[^/]+\/watch$/.test(bare)) {
      return {
        status: 200,
        body: { paused: watch.paused, pending_events: watch.pending, ...watch.extra },
      };
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
  const dispatch = vi.fn(async (_invocation: unknown, _context?: unknown) => ({ ok: true }));
  const created: string[] = [];
  const handlers = new Map<string, (...a: string[]) => unknown>();
  /** Every value-picker session the panel opened (doc "Value picker"). */
  const picks: Record<string, unknown>[] = [];
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
      case 'pick_start':
        picks.push((args!.spec as Record<string, unknown>));
        return 'ws-2';
      case 'workspace_create':
        created.push(args!.activeRepo as string);
        return 'ws-new';
      case 'post_status':
      case 'append_message':
      case 'suggest_keybinding':
      case 'bench_record':
      case 'ref_seed':
      case 'fs_home_dir':
        return command === 'fs_home_dir' ? '/home/user' : null;
      default:
        throw new Error(`unexpected Tauri command: ${command}`);
    }
  });

  const provide = vi.fn();
  const instance = createPanelApi(
    {
      invoke,
      dispatch,
      // As the shell's dispatcher runs it: given the API of the run first.
      registerHandler: (name, handler) =>
        handlers.set(name, (...args: string[]) =>
          handler(instance.forRun({ ws: 'ws-1', slot: 'left' }), ...args),
        ),
      registerArgs: () => {},
      onCommandsChanged: () => {},
      addDefaultMenuItems: () => {},
      provide,
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
  return {
    api: instance.api,
    daemon,
    vars,
    dispatch,
    invoke,
    handlers,
    initRepo,
    picks,
    created,
    provide,
    pushVarChanged: instance.pushVarChanged,
  };
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
    // A fresh realm per test: the change feed is a module-level singleton
    // shared by every panel, so one test's baselines must not leak into the next.
    changeFeed._reset();
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
    expect(dispatch).toHaveBeenCalledWith(['panel:open', 'here', 'metarecord-list'], {
      ws: 'ws-1',
      slot: 'left',
    });
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
    const { api, daemon, dispatch, vars, created } = setup({ activeRepo: 'other-repo' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    (shadow.querySelector('#repo-list .repo-head') as HTMLElement).click();
    await settle();

    // An empty workspace on the repository, the list opened in it.
    expect(created).toEqual(['r1']);
    expect(dispatch).toHaveBeenCalledWith(
      ['panel:open', 'here', 'metarecord-list'],
      expect.objectContaining({ ws: 'ws-new' }),
    );
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

describe('repos panel — a lost repository', () => {
  beforeEach(() => changeFeed._reset());

  test('is listed, marked, says how to resume, and cannot be opened', async () => {
    const { api, daemon, dispatch, created, calls } = (() => {
      const s = setup({ activeRepo: null });
      return { ...s, calls: s.daemon.calls };
    })();
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos', lost: true });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const row = shadow.querySelector('#repo-list > li') as HTMLElement;
    expect(row).not.toBeNull();
    expect(row.classList.contains('lost')).toBe(true);
    expect(row.textContent).toContain('mf repo load');
    // Nothing to retype in, nothing to open, no watcher to ask about.
    expect(row.textContent).not.toContain('Retype');
    (row.querySelector('.repo-head') as HTMLElement).click();
    await settle();
    expect(dispatch).not.toHaveBeenCalled();
    expect(created).toEqual([]);
    expect(calls.some((c) => c.path === '/repos/r1/watch')).toBe(false);
  });

  test('forgetting it unloads it', async () => {
    const { api, daemon } = setup({ activeRepo: null });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos', lost: true });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const forget = shadow.querySelector('#repo-list .repo-unload') as HTMLElement;
    expect(forget.textContent).toBe('Forget');
    forget.click();
    await settle();
    expect(daemon.repos).toHaveLength(0);
  });
});

describe('repos panel — a repository still loading', () => {
  beforeEach(() => changeFeed._reset());
  // Even when a test fails midway: fake timers would stall every later test.
  afterEach(() => vi.useRealTimers());

  test('is marked and cannot be opened until it is ready', async () => {
    const { api, daemon, dispatch, created, provide } = setup({ activeRepo: 'other' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos', ready: false });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const row = shadow.querySelector('#repo-list > li') as HTMLElement;
    expect(row.classList.contains('loading')).toBe(true);
    expect(row.textContent).toContain('loading');
    (row.querySelector('.repo-head') as HTMLElement).click();
    await settle();
    expect(created).toEqual([]);
    expect(dispatch).not.toHaveBeenCalled();

    // Once loaded, the same click opens it.
    daemon.repos[0].ready = true;
    await generic(provide, 'view:refresh');
    await settle();
    const ready = shadow.querySelector('#repo-list > li') as HTMLElement;
    expect(ready.classList.contains('loading')).toBe(false);
    (ready.querySelector('.repo-head') as HTMLElement).click();
    await settle();
    expect(created).toEqual(['r1']);
  });

  test('the row becomes openable by itself when the load finishes', async () => {
    vi.useFakeTimers();
    const { api, daemon } = setup({ activeRepo: 'other' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos', ready: false });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await vi.advanceTimersByTimeAsync(0);
    expect(shadow.querySelector('#repo-list > li.loading')).not.toBeNull();

    daemon.repos[0].ready = true;
    await vi.advanceTimersByTimeAsync(1600); // one task poll
    expect(shadow.querySelector('#repo-list > li.loading')).toBeNull();
  });

  test('a repository loaded from the form is adopted once it is ready, not before', async () => {
    vi.useFakeTimers();
    const { api, daemon, vars, dispatch } = setup({ activeRepo: null });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos', ready: false });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await vi.advanceTimersByTimeAsync(0);

    openForm(shadow, 'show-load');
    submitForm(shadow, 'load-form', { 'load-root': '/tmp/photos' });
    await vi.advanceTimersByTimeAsync(0);
    expect(vars.get('active_repo')).toBeNull();
    expect(dispatch).not.toHaveBeenCalled();

    daemon.repos[0].ready = true;
    await vi.advanceTimersByTimeAsync(1600);
    expect(vars.get('active_repo')).toBe('r1');
    expect(dispatch).toHaveBeenCalledWith(['panel:open', 'here', 'metarecord-list'], {
      ws: 'ws-1',
      slot: 'left',
    });
  });
});

describe('repos panel — running tasks', () => {
  test('an in-flight task is shown under its repository and can be stopped', async () => {
    const { api, provide, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.tasks.push({
      id: 't1',
      repo_uuid: 'r1',
      kind: 'reconcile',
      cancellable: true,
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
    await generic(provide, 'view:refresh');
    await settle();
    expect(shadow.querySelectorAll('.repo-tasks .repo-task')).toHaveLength(0);
  });

  // The daemon says which tasks can be stopped; the panel's own list of kinds
  // had missed the duplicate scan.
  test('a duplicate scan in flight can be stopped too', async () => {
    const { api, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.tasks.push({
      id: 't3',
      repo_uuid: 'r1',
      kind: 'duplicates',
      cancellable: true,
      status: 'running',
      phase: 'full',
      done: 1,
      total: 9,
    });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    (shadow.querySelector('.repo-task .task-stop') as HTMLElement).click();
    await settle();
    expect(daemon.cancelled).toEqual(['t3']);
  });

  test('a task that cannot be cancelled gets no Stop button', async () => {
    const { api, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.tasks.push({
      id: 't2',
      repo_uuid: 'r1',
      kind: 'load',
      cancellable: false,
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
      cancellable: true,
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

describe('repos panel — watch sources', () => {
  test('each mount the broker cannot cover is named, with why', async () => {
    const { api, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.watch.extra = {
      backend: 'fanotify',
      uncovered: {
        mounts: [{ path: '/enc', reason: 'the filesystem has no fsid' }],
        watched_dirs: 3,
      },
    };

    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const notice = shadow.querySelector('.repo-watch .watch-uncovered') as HTMLElement;
    expect(notice?.textContent).toContain('/enc');
    expect(notice.textContent).toContain('inotify');
    expect(notice.title).toContain('the filesystem has no fsid');
    expect(notice.title).toContain('3');
  });

  test('a repository off the broker says it is on inotify, and why', async () => {
    const { api, daemon } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    daemon.watch.extra = {
      backend: 'inotify',
      backend_reason: 'connecting to /run/watchd.sock: refused',
      uncovered: null,
    };

    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const notice = shadow.querySelector('.repo-watch .watch-fallback') as HTMLElement;
    expect(notice?.textContent).toContain('inotify');
    expect(notice.title).toContain('connecting to /run/watchd.sock: refused');
    expect(shadow.querySelector('.watch-uncovered')).toBeNull();
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
  test('open-init reveals its form', async () => {
    const { api, handlers } = setup();
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await command(handlers, 'repos:open-init')();
    expect((shadow.getElementById('init-form') as HTMLElement).classList.contains('hidden')).toBe(
      false,
    );
    // Cancel closes the form again.
    (shadow.querySelector('#init-form .cancel') as HTMLElement).click();
    expect((shadow.getElementById('init-form') as HTMLElement).classList.contains('hidden')).toBe(
      true,
    );
  });

  // `repos:init` is the form's keyboard and script path: the folder and the
  // name as arguments, the same creation (repo_init, ignore preset included).
  test('init creates the repository from its arguments and adopts it', async () => {
    const { api, daemon, vars, dispatch, handlers, initRepo } = setup({ activeRepo: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await command(handlers, 'repos:init')('/tmp/photos', 'pics');
    await settle();

    expect(initRepo).toHaveBeenCalledWith(
      expect.objectContaining({ root: '/tmp/photos', name: 'pics' }),
    );
    expect(daemon.repos.map((r) => r.name)).toEqual(['pics']);
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(1);
    expect(vars.get('active_repo')).toBe(daemon.repos[0].repo_uuid);
    expect(dispatch).toHaveBeenCalledWith(['panel:open', 'here', 'metarecord-list'], {
      ws: 'ws-1',
      slot: 'left',
    });
  });

  test('init without a name leaves it to the daemon, and announces the repository', async () => {
    const { api, daemon, handlers, initRepo, invoke } = setup({ activeRepo: 'r0' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await command(handlers, 'repos:init')('/tmp/photos');
    await settle();

    expect(initRepo).toHaveBeenCalledWith(expect.objectContaining({ root: '/tmp/photos' }));
    expect(initRepo.mock.calls[0][0].name ?? null).toBe(null);
    expect(daemon.repos.map((r) => r.name)).toEqual(['photos']);
    // The workspace has its repository already: the new one is announced.
    expect(
      invoke.mock.calls.some(
        ([name, args]) =>
          name === 'post_status' &&
          String((args as { text?: string }).text ?? '').startsWith('Repository ready'),
      ),
    ).toBe(true);
  });

  test('a refused init is a failure of the command', async () => {
    const { api, handlers, initRepo } = setup();
    initRepo.mockRejectedValueOnce(new Error('already a repository'));
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await expect(command(handlers, 'repos:init')('/tmp/photos')).rejects.toThrow(
      'already a repository',
    );
  });

  // `repos:load` is the keyboard path: no form to fill, just the folder picker
  // and the load — the typed-path form stays behind the "Load repo" button.
  test('load opens the folder picker and loads what was picked', async () => {
    const { api, daemon, vars, handlers, picks, pushVarChanged } = setup({ activeRepo: null });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    daemon.calls.length = 0;

    const running = command(handlers, 'repos:load')('on-disk');
    await settle();
    expect(picks).toHaveLength(1);
    expect((picks[0].panel as { type: string }).type).toBe('file-manager');
    expect(picks[0].result).toBe('path');
    expect(picks[0].repo).toBe(null); // the folder is not a repo of ours yet

    pushVarChanged('pick_result', { token: picks[0].token, path: '/tmp/photos' });
    await running;
    await settle();

    expect(
      daemon.calls.some(
        (c) => c.path === '/repos/load' && (c.body as { root: string }).root === '/tmp/photos',
      ),
    ).toBe(true);
    // A workspace with no repository adopts the one just loaded.
    expect(vars.get('active_repo')).toBe('r1');
    // No form was opened on the way.
    expect((shadow.getElementById('load-form') as HTMLElement).classList.contains('hidden')).toBe(
      true,
    );
  });

  test('cancelling the folder picker loads nothing', async () => {
    const { api, daemon, handlers, picks, pushVarChanged } = setup({ activeRepo: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    daemon.calls.length = 0;

    const running = command(handlers, 'repos:load')('on-disk');
    await settle();
    pushVarChanged('pick_result', { token: picks[0].token, cancelled: true });
    await running;
    await settle();

    expect(daemon.calls.some((c) => c.path === '/repos/load')).toBe(false);
  });

  test('a folder that is not a repository is reported in the status bar', async () => {
    const { api, daemon, handlers, picks, pushVarChanged, invoke } = setup({ activeRepo: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const running = command(handlers, 'repos:load')('on-disk');
    await settle();
    pushVarChanged('pick_result', { token: picks[0].token, path: '/tmp/not-a-repo' });
    await running;
    await settle();

    expect(daemon.repos).toHaveLength(0);
    // The picker left no form on screen, so the error has to be said out loud.
    expect(
      invoke.mock.calls.some(
        ([name, args]) =>
          name === 'post_status' &&
          String((args as { text?: string }).text ?? '').includes('not a repository'),
      ),
    ).toBe(true);
  });

  test('refresh picks up a repository loaded from elsewhere', async () => {
    const { api, provide, daemon } = setup();
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(0);

    daemon.repos.push({ repo_uuid: 'r9', name: 'later', root: '/tmp/later' });
    await generic(provide, 'view:refresh');
    await settle();
    expect(shadow.querySelectorAll('#repo-list > li')).toHaveLength(1);
  });
});

// doc "Holding a repository in memory": the store read into RAM at load.
describe('repos panel — in memory', () => {
  beforeEach(() => {
    changeFeed._reset();
  });

  const loadedWithMemory = (daemon: ReturnType<typeof fakeDaemon>) =>
    daemon.calls.filter(
      (c) => c.path === '/repos/load' && (c.body as { in_memory?: boolean }).in_memory === true,
    );

  test('the load form loads in memory from its own button', async () => {
    const { api, daemon, vars } = setup({ activeRepo: null });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    openForm(shadow, 'show-load');
    (shadow.getElementById('load-root') as HTMLInputElement).value = '/tmp/photos';
    (shadow.getElementById('load-memory') as HTMLButtonElement).click();
    await settle();

    expect(loadedWithMemory(daemon)).toHaveLength(1);
    expect((loadedWithMemory(daemon)[0].body as { root: string }).root).toBe('/tmp/photos');
    expect(vars.get('active_repo')).toBe('r1');
  });

  test('the ordinary Load does not ask for memory', async () => {
    const { api, daemon } = setup({ activeRepo: null });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    openForm(shadow, 'show-load');
    submitForm(shadow, 'load-form', { 'load-root': '/tmp/photos' });
    await settle();

    expect(daemon.calls.some((c) => c.path === '/repos/load')).toBe(true);
    expect(loadedWithMemory(daemon)).toHaveLength(0);
  });

  test('repos:load in-memory loads the picked folder in memory', async () => {
    const { api, daemon, handlers, picks, pushVarChanged } = setup({ activeRepo: null });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const running = command(handlers, 'repos:load')('in-memory');
    await settle();
    pushVarChanged('pick_result', { token: picks[0].token, path: '/tmp/photos' });
    await running;
    await settle();

    expect(loadedWithMemory(daemon)).toHaveLength(1);
  });

  test('repos:load on-disk does not ask for memory', async () => {
    const { api, daemon, handlers, picks, pushVarChanged } = setup({ activeRepo: null });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos' });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const running = command(handlers, 'repos:load')('on-disk');
    await settle();
    pushVarChanged('pick_result', { token: picks[0].token, path: '/tmp/photos' });
    await running;
    await settle();

    expect(daemon.calls.some((c) => c.path === '/repos/load')).toBe(true);
    expect(loadedWithMemory(daemon)).toHaveLength(0);
  });

  test('repos:load without a mode is an error, and picks nothing', async () => {
    const { api, handlers, picks } = setup({ activeRepo: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    await expect(command(handlers, 'repos:load')()).rejects.toThrow('on-disk');
    expect(picks).toHaveLength(0);
  });

  test('repos:load with an unknown mode is an error, and picks nothing', async () => {
    const { api, handlers, picks } = setup({ activeRepo: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    await expect(command(handlers, 'repos:load')('in-ram')).rejects.toThrow('in-memory');
    expect(picks).toHaveLength(0);
  });

  test('a repository held in memory says so on its row, and why it is not locked', async () => {
    const { api, daemon } = setup();
    daemon.repos.push(
      {
        repo_uuid: 'r1',
        name: 'pinned',
        root: '/tmp/a',
        in_memory: { bytes: 3 << 20, pinned: true, reason: null },
      },
      {
        repo_uuid: 'r2',
        name: 'preloaded',
        root: '/tmp/b',
        in_memory: { bytes: 5 << 20, pinned: false, reason: 'the limit is 8 MiB' },
      },
      { repo_uuid: 'r3', name: 'plain', root: '/tmp/c', in_memory: null },
    );
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    const badges = [...shadow.querySelectorAll('#repo-list > li')].map((li) =>
      li.querySelector<HTMLElement>('.memory'),
    );
    expect(badges[0]?.textContent).toBe('in memory');
    expect(badges[0]?.title).toContain('3 MiB');
    expect(badges[1]?.textContent).toBe('in memory, not locked');
    expect(badges[1]?.title).toContain('the limit is 8 MiB');
    expect(badges[1]?.dataset.unpinned).toBe('');
    expect(badges[2]).toBeNull();
  });

  const memoryCalls = (daemon: ReturnType<typeof fakeDaemon>) =>
    daemon.calls
      .filter((c) => c.path.endsWith('/memory'))
      .map((c) => [c.path, (c.body as { in_memory: boolean }).in_memory]);

  test('repos:toggle-memory holds the active repository in memory, then lets it go', async () => {
    const { api, daemon, handlers } = setup({ activeRepo: 'r1' });
    daemon.repos.push({ repo_uuid: 'r1', name: 'photos', root: '/tmp/photos', in_memory: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await command(handlers, 'repos:toggle-memory')();
    await settle();
    expect(shadow.querySelector('#repo-list .memory')?.textContent).toBe('in memory');
    await command(handlers, 'repos:toggle-memory')();
    await settle();
    expect(shadow.querySelector('#repo-list .memory')).toBeNull();
    expect(memoryCalls(daemon)).toEqual([
      ['/repos/r1/memory', true],
      ['/repos/r1/memory', false],
    ]);
  });

  test('repos:toggle-memory takes a repository by name', async () => {
    const { api, daemon, handlers } = setup({ activeRepo: 'r1' });
    daemon.repos.push(
      { repo_uuid: 'r1', name: 'photos', root: '/tmp/photos', in_memory: null },
      {
        repo_uuid: 'r2',
        name: 'music',
        root: '/tmp/music',
        in_memory: { bytes: 1 << 20, pinned: true, reason: null },
      },
    );
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();

    await command(handlers, 'repos:toggle-memory')('music');
    expect(memoryCalls(daemon)).toEqual([['/repos/r2/memory', false]]);
    await expect(command(handlers, 'repos:toggle-memory')('nope')).rejects.toThrow('nope');
  });

  test('repos:toggle-memory without a repository is an error', async () => {
    const { api, daemon, handlers } = setup({ activeRepo: null });
    const shadow = shadowForRepos();
    await mount(shadow, api);
    await settle();
    await expect(command(handlers, 'repos:toggle-memory')()).rejects.toThrow('no active');
    expect(memoryCalls(daemon)).toEqual([]);
  });
});
