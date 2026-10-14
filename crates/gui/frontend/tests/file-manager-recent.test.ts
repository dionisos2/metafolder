// The file manager's half of the frecent folders (doc "Frecent folders"):
// going into a folder is a view of that folder's metarecord, recorded in the
// repository's recently-viewed list like a view in the detail panel — once per
// arrival, not again when the same folder is listed again (a refresh, a
// mutation, the change feed).

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
  { name: 'sub', path: '/srv/sub', is_dir: true },
  { name: 'song.mp3', path: '/srv/song.mp3', is_dir: false },
];

function stub(repo: string | null) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const varListeners = new Map<string, ((v: unknown) => void)[]>();
  const subscribers: ChangeCb[] = [];

  // Branches on the endpoint so the directory resolves to a node and the child
  // is reported tracked — every call is counted, so a re-enrich is observable.
  const daemonCall = vi.fn(
    async (method: string, path: string, body?: { path?: string }): Promise<unknown> => {
      if (path.includes('/tree/resolve-path')) {
        return { uuid: body?.path === '/sub' ? 'subuuid' : 'diruuid' };
      }
      if (path.endsWith('/watch')) return { quiet_period_ms: 3000 };
      if (path.includes('/tree/children')) return [{ uuid: 'songuuid', name: 'song.mp3' }];
      return { results: [], next_cursor: null };
    },
  );

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
      repoRoot: async () => '/srv',
      repoInternalDir: async () => '/srv/.metafolder/internal',
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
    provide: vi.fn(),
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
    recent: { touch: vi.fn(async () => {}), list: vi.fn(async () => []) },
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

async function mount(repo: string | null) {
  const s = stub(repo);
  const root = shadowRoot();
  const mod = await import('../../default-config/panel-types/file-manager/main.js');
  await mod.mount(root, s.api as never);
  await new Promise((r) => setTimeout(r, 0));
  return { ...s, root };
}

/** What the panel handed `metafolder.provide`, merged. */
function provided(api: { provide: { mock: { calls: unknown[][] } } }) {
  return Object.assign({}, ...api.provide.mock.calls.map((c) => c[0])) as {
    refresh: () => Promise<void>;
    descend: (index: number) => Promise<boolean>;
    entries: () => { name: string; index: number }[];
  };
}

describe('file-manager records the folders it goes into', () => {
  beforeEach(() => {
    vi.stubGlobal('prompt', vi.fn());
    vi.stubGlobal('confirm', vi.fn(() => true));
    Element.prototype.scrollIntoView = () => {};
  });

  test('the folder shown on mount is recorded', async () => {
    const { api } = await mount('r1');
    expect(api.recent.touch.mock.calls).toEqual([['r1', 'diruuid']]);
  });

  test('a refresh of the same folder records nothing more', async () => {
    const { api } = await mount('r1');
    await provided(api).refresh();
    await new Promise((r) => setTimeout(r, 0));
    expect(api.recent.touch.mock.calls).toEqual([['r1', 'diruuid']]);
  });

  test('going into another folder records it', async () => {
    const { api } = await mount('r1');
    const p = provided(api);
    const sub = p.entries().find((row) => row.name === 'sub');
    expect(await p.descend(sub!.index)).toBe(true);
    await new Promise((r) => setTimeout(r, 0));
    expect(api.recent.touch.mock.calls.at(-1)).toEqual(['r1', 'subuuid']);
  });

  test('an untracked folder records nothing', async () => {
    const s = stub('r1');
    s.daemonCall.mockImplementation(async (_m: string, path: string) =>
      path.includes('/tree/resolve-path') ? { uuid: null } : [],
    );
    const mod = await import('../../default-config/panel-types/file-manager/main.js');
    await mod.mount(shadowRoot(), s.api as never);
    await new Promise((r) => setTimeout(r, 0));
    expect(s.api.recent.touch).not.toHaveBeenCalled();
  });
});
