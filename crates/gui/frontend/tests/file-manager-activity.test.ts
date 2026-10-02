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

function stub(
  repo: string | null,
  activity: Record<string, number> | null,
  operations: Record<string, number> | null = null,
) {
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
      const byOps = path.includes('sort=operations');
      const children = [...new Set([...Object.keys(counts), ...Object.keys(operations ?? {})])]
        .filter((p) => p !== '' && p.lastIndexOf('/') === 0)
        .map((p) => ({
          path: p,
          events: counts[p] ?? 0,
          ...(operations && { operations: operations[p] ?? 0 }),
        }))
        .sort((a, b) =>
          byOps ? (b.operations ?? 0) - (a.operations ?? 0) : b.events - a.events,
        );
      return { since_ms: 0, total: counts[''] ?? 0, path: '', events: counts[''] ?? 0, children };
    }
    if (path.endsWith('/watch/activity')) {
      if (activity === null) throw new Error('no such endpoint');
      return {
        since_ms: 0,
        total: activity[''] ?? 0,
        ...(operations && { total_operations: operations[''] ?? 0 }),
        results: ((body as { paths: string[] }).paths).map((p) => ({
          path: p,
          events: activity[p] ?? 0,
          ...(operations && { operations: operations[p] ?? 0 }),
        })),
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

async function mount(
  repo: string | null,
  activity: Record<string, number> | null,
  operations: Record<string, number> | null = null,
) {
  const s = stub(repo, activity, operations);
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

  test('each row carries the operations written for it, beside its events', async () => {
    const { root } = await mount(
      'r1',
      { '': 1000, '/build': 900, '/song.mp3': 3 },
      { '': 40, '/build': 2, '/song.mp3': 38 },
    );
    const song = row(root, 'song.mp3').querySelector('.operations') as HTMLElement;
    expect(song.textContent).toBe('38 op');
    expect(song.title).toContain('38 operation(s) written to the log');
    expect(song.title).toContain('95% of all the watcher wrote');
    expect(row(root, 'build').querySelector('.operations')?.textContent).toBe('2 op');
    // The events are still there, and still what marks a hot spot.
    expect(row(root, 'build').querySelector('.activity')?.textContent).toBe('900');
    expect(row(root, 'build').classList.contains('hot')).toBe(true);
    expect(row(root, 'song.mp3').classList.contains('hot')).toBe(false);
    // Nothing written, or a daemon that does not count them: nothing shown.
    expect(row(root, 'zcache').querySelector('.operations')?.textContent ?? '').toBe('');
    const older = await mount('r1', { '': 1000, '/build': 900 });
    expect(row(older.root, 'build').querySelector('.operations')?.textContent ?? '').toBe('');
  });

  test('sorting by operations puts what wrote the most first', async () => {
    const { root, handlers } = await mount(
      'r1',
      { '': 1000, '/zcache': 700, '/song.mp3': 200 },
      { '': 40, '/song.mp3': 30, '/build': 10 },
    );
    await handlers.get('file-manager:toggle')!('operations');
    expect(names(root)).toEqual(['.', 'song.mp3', 'build', 'zcache']);
    expect((root.getElementById('sort-operations') as HTMLInputElement).checked).toBe(true);

    await handlers.get('file-manager:toggle')!('operations');
    expect(names(root)).toEqual(['.', 'build', 'song.mp3', 'zcache']);
  });

  test('the two orders exclude each other', async () => {
    const { root, handlers } = await mount(
      'r1',
      { '': 1000, '/zcache': 700, '/song.mp3': 200 },
      { '': 40, '/song.mp3': 30, '/build': 10 },
    );
    const byActivity = root.getElementById('sort-activity') as HTMLInputElement;
    const byOperations = root.getElementById('sort-operations') as HTMLInputElement;

    await handlers.get('file-manager:toggle')!('activity');
    await handlers.get('file-manager:toggle')!('operations');
    expect(byActivity.checked).toBe(false);
    expect(byOperations.checked).toBe(true);
    expect(names(root)).toEqual(['.', 'song.mp3', 'build', 'zcache']);

    await handlers.get('file-manager:toggle')!('activity');
    expect(byActivity.checked).toBe(true);
    expect(byOperations.checked).toBe(false);
    expect(names(root)).toEqual(['.', 'zcache', 'song.mp3', 'build']);
  });

  // What `mf:watch-activity reset` relies on: the reset is in no log, so the
  // shell raises a whole-repository change, and the counts must be re-read.
  test('a whole-repository change re-reads the counts', async () => {
    const counts: Record<string, number> = { '': 1000, '/build': 900 };
    const { root, fireChange } = await mount('r1', counts);
    expect(row(root, 'build').querySelector('.activity')?.textContent).toBe('900');

    for (const path of Object.keys(counts)) delete counts[path];
    fireChange({ repo: 'r1', uuids: null });
    await new Promise((r) => setTimeout(r, 0));
    await new Promise((r) => setTimeout(r, 0));

    expect(row(root, 'build').querySelector('.activity')?.textContent ?? '').toBe('');
    expect(row(root, 'build').classList.contains('hot')).toBe(false);
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
