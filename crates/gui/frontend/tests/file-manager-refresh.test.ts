// file-manager live refresh (spec-gui "file-manager panel type"): the panel
// must re-query its directory's tracked status when the daemon's change feed
// reports a change (an external rename, or the watcher repairing the
// metarecord↔file link ~500 ms after a GUI rename), and it must schedule
// catch-up syncs after a local mutation so that delayed watcher repair is
// picked up promptly rather than only on the 7 s background poll. Without this
// the tracked badge stays stale after a rename.

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

const DIR_ENTRIES = [{ name: 'song.mp3', path: '/song.mp3', is_dir: false }];

function stub(repo: string | null) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const varListeners = new Map<string, ((v: unknown) => void)[]>();
  const subscribers: ChangeCb[] = [];

  // Branches on the endpoint so the directory resolves to a node and the child
  // is reported tracked — every call is counted, so a re-enrich is observable.
  const daemonCall = vi.fn(async (method: string, path: string) => {
    if (path.includes('/tree/resolve-path')) return { uuid: 'diruuid' };
    if (path.includes('/tree/children')) return [{ uuid: 'songuuid', name: 'song.mp3' }];
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
      call: daemonCall,
      repoRoot: async () => '/',
      repoInternalDir: async () => '/.metafolder/internal',
    },
    cache: {
      sync: vi.fn(async () => {}),
      subscribe: vi.fn((cb: ChangeCb) => {
        subscribers.push(cb);
        return () => {
          const i = subscribers.indexOf(cb);
          if (i >= 0) subscribers.splice(i, 1);
        };
      }),
    },
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

async function mount(repo: string | null) {
  const s = stub(repo);
  const root = shadowRoot();
  const mod = await import('../../default-config/panel-types/file-manager/main.js');
  await mod.mount(root, s.api as never);
  await new Promise((r) => setTimeout(r, 0));
  return { ...s, root };
}

describe('file-manager live refresh', () => {
  beforeEach(() => {
    vi.stubGlobal('prompt', vi.fn());
    vi.stubGlobal('confirm', vi.fn(() => true));
    Element.prototype.scrollIntoView = () => {};
  });

  test('re-queries tracked status when the change feed reports a change', async () => {
    const { daemonCall, fireChange } = await mount('r1');
    // The initial open resolved the directory + its children.
    expect(daemonCall).toHaveBeenCalled();
    daemonCall.mockClear();

    // The watcher repaired the metarecord ~500 ms after a rename: the change
    // feed fires. The panel must re-enrich the visible directory.
    fireChange({ repo: 'r1', uuids: null });
    await new Promise((r) => setTimeout(r, 0));

    expect(daemonCall).toHaveBeenCalledWith(
      'GET',
      expect.stringContaining('/tree/children'),
    );
  });

  test('ignores change events for another repo', async () => {
    const { daemonCall, fireChange } = await mount('r1');
    daemonCall.mockClear();
    fireChange({ repo: 'other', uuids: null });
    await new Promise((r) => setTimeout(r, 0));
    expect(daemonCall).not.toHaveBeenCalled();
  });

  test('schedules catch-up syncs after a mutation so the watcher repair is picked up', async () => {
    vi.useFakeTimers();
    try {
      const s = stub('r1');
      const root = shadowRoot();
      const mod = await import('../../default-config/panel-types/file-manager/main.js');
      await mod.mount(root, s.api as never);
      await vi.runOnlyPendingTimersAsync();
      s.api.cache.sync.mockClear();

      // A mutation elsewhere (or here) nudges the panel via metarecords:dirty.
      s.fireVar('metarecords:dirty', Date.now());
      await vi.advanceTimersByTimeAsync(0); // immediate sync in onMetarecordsDirty
      const immediate = s.api.cache.sync.mock.calls.length;

      // The catch-up nudges fire after the watcher's quiet period (~500 ms) so
      // the repaired link is reflected well before the 7 s background poll.
      await vi.advanceTimersByTimeAsync(2000);
      expect(s.api.cache.sync.mock.calls.length).toBeGreaterThan(immediate);
    } finally {
      vi.useRealTimers();
    }
  });
});
