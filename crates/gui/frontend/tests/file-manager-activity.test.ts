// file-manager watch activity (doc "Watch activity"): each row shows how
// many watcher events arrived under it since the load (recursive counts, the
// "." row being the directory's own), so the user can walk down from the root
// to where the events come from.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/file-manager');

function shadowRoot(): ShadowRoot {
  const html = readFileSync(resolve(PANEL_DIR, 'index.html'), 'utf8');
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

type Handler = (...args: string[]) => unknown;
type ChangeCb = (event: { repo: string; uuids: string[] | null }) => void;

const DIR_ENTRIES = [
  { name: 'build', path: '/r/build', is_dir: true },
  { name: 'song.mp3', path: '/r/song.mp3', is_dir: false },
  { name: 'zcache', path: '/r/zcache', is_dir: true },
];

function stub(repo: string | null, activity: Record<string, number> | null) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const varListeners = new Map<string, ((v: unknown) => void)[]>();
  const subscribers: ChangeCb[] = [];

  // Branches on the endpoint so the directory resolves to a node and the child
  // is reported tracked — every call is counted, so a re-enrich is observable.
  const daemonCall = vi.fn(async (method: string, path: string, body?: unknown) => {
    if (path.includes('/tree/resolve-path')) return { uuid: 'diruuid' };
    if (path.endsWith('/watch')) return { quiet_period_ms: 3000 };
    if (path.includes('/tree/children')) return [{ uuid: 'songuuid', name: 'song.mp3' }];
    if (path.startsWith('/repos/r1/watch/activity?')) {
      if (activity === null) throw new Error('no such endpoint');
      const counts = activity;
      const children = Object.keys(counts)
        .filter((p) => p !== '' && p.lastIndexOf('/') === 0)
        .map((p) => ({ path: p, events: counts[p] }))
        .sort((a, b) => b.events - a.events);
      return { since_ms: 0, total: counts[''] ?? 0, path: '', events: counts[''] ?? 0, children };
    }
    if (path.endsWith('/watch/activity')) {
      if (activity === null) throw new Error('no such endpoint');
      return {
        since_ms: 0,
        total: activity[''] ?? 0,
        results: ((body as { paths: string[] }).paths).map((p) => ({ path: p, events: activity[p] ?? 0 })),
      };
    }
    return { results: [], next_cursor: null };
  });

  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'file-manager',
    pageSize: 100,
    settings: { statusMessageMs: 1000, statusErrorMs: 2000 },
    defaults: {},
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => void) => fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
      metarecords: async () => new Map(),
      fields: async () => [],
      call: daemonCall,
      repoRoot: async () => '/r',
      repoInternalDir: async () => '/r/.metafolder/internal',
    },
    changes: { sync: vi.fn(async () => {}), subscribe: vi.fn((cb: ChangeCb) => {
        subscribers.push(cb);
        return () => {
          const i = subscribers.indexOf(cb);
          if (i >= 0) subscribers.splice(i, 1);
        };
      }) },
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? repo : null),
      set: vi.fn(async () => {}),
      onChange(key: string, listener: (v: unknown) => void) {
        const list = varListeners.get(key) ?? [];
        list.push(listener);
        varListeners.set(key, list);
      },
    },
    commands: {
      register: (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return Promise.resolve(null);
      },
      invoke: () => null,
    },
    fs: {
      readDir: vi.fn(async () => DIR_ENTRIES.map((e) => ({ ...e }))),
      stat: vi.fn(async () => ({})),
    exists: vi.fn(async () => true),
      homeDir: vi.fn(async () => '/home/user'),
      mkdir: vi.fn(async () => {}),
      createFile: vi.fn(async () => {}),
      move: vi.fn(async () => {}),
      copy: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    },
    trash: { trashPath: vi.fn(async () => 'song.mp3') },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  const fireVar = (key: string, value: unknown) => {
    for (const l of varListeners.get(key) ?? []) l(value);
  };
  const fireChange = (event: { repo: string; uuids: string[] | null }) => {
    for (const cb of [...subscribers]) cb(event);
  };
  return { api, handlers, daemonCall, fireVar, fireChange };
}

async function mount(repo: string | null, activity: Record<string, number> | null) {
  const s = stub(repo, activity);
  const root = shadowRoot();
  const mod = await import('../../default-config/panel-types/file-manager/main.js');
  await mod.mount(root, s.api as never);
  await new Promise((r) => setTimeout(r, 0));
  return { ...s, root };
}

/** The rendered row named `name`. */
function row(root: ShadowRoot, name: string): HTMLElement {
  const rows = [...root.querySelectorAll('#entries li')] as HTMLElement[];
  const found = rows.find((li) => li.querySelector('.name')?.textContent === name);
  if (!found) throw new Error(`no row ${name}`);
  return found;
}

describe('file-manager watch activity', () => {
  beforeEach(() => {
    vi.stubGlobal('prompt', vi.fn());
    vi.stubGlobal('confirm', vi.fn(() => true));
    Element.prototype.scrollIntoView = () => {};
  });

  test('each row carries its count, the hot spot is marked', async () => {
    const { root, daemonCall } = await mount('r1', { '': 1000, '/build': 900, '/song.mp3': 3 });
    const call = daemonCall.mock.calls.find((c) => c[1] === '/repos/r1/watch/activity');
    expect(call?.[2]).toEqual({ paths: ['', '/build', '/song.mp3', '/zcache'] });

    const build = row(root, 'build').querySelector('.activity') as HTMLElement;
    expect(build.textContent).toBe('900');
    expect(build.title).toContain('90% of all events');
    expect(row(root, 'build').classList.contains('hot')).toBe(true);
    expect(row(root, 'song.mp3').querySelector('.activity')?.textContent).toBe('3');
    expect(row(root, 'song.mp3').classList.contains('hot')).toBe(false);
    expect(row(root, '.').querySelector('.activity')?.textContent).toBe('1.0k');
  });

  test('a quiet row shows nothing, and so does an unanswered call', async () => {
    const quiet = await mount('r1', { '': 5, '/build': 5 });
    expect(row(quiet.root, 'song.mp3').querySelector('.activity')?.textContent ?? '').toBe('');
    const unknown = await mount('r1', null);
    expect(row(unknown.root, 'build').querySelector('.activity')?.textContent ?? '').toBe('');
  });

  /** The names of the rendered rows, in order. */
  const names = (root: ShadowRoot) =>
    [...root.querySelectorAll('#entries li .name')].map((n) => n.textContent);

  test('sorting by activity puts the busiest entries first, "." stays on top', async () => {
    const { root, handlers } = await mount('r1', { '': 1000, '/zcache': 700, '/song.mp3': 200 });
    expect(names(root)).toEqual(['.', 'build', 'song.mp3', 'zcache']);
    await handlers.get('file-manager:toggle')!('activity');
    expect(names(root)).toEqual(['.', 'zcache', 'song.mp3', 'build']);
    expect((root.getElementById('sort-activity') as HTMLInputElement).checked).toBe(true);
    // The counts follow the rows they belong to.
    expect(row(root, 'zcache').querySelector('.activity')?.textContent).toBe('700');

    await handlers.get('file-manager:toggle')!('activity');
    expect(names(root)).toEqual(['.', 'build', 'song.mp3', 'zcache']);
  });

  test('the checkbox toggles the same sort', async () => {
    const { root } = await mount('r1', { '': 1000, '/zcache': 700 });
    const box = root.getElementById('sort-activity') as HTMLInputElement;
    box.checked = true;
    box.dispatchEvent(new Event('change'));
    await new Promise((r) => setTimeout(r, 0));
    await new Promise((r) => setTimeout(r, 0));
    expect(names(root)).toEqual(['.', 'zcache', 'build', 'song.mp3']);
  });
});
