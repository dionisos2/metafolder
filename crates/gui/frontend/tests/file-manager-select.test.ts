// The file manager joins the checked multi-selection (doc "file-manager panel",
// doc "Cross-panel selection"): a tracked row can be checked into
// `selected_metarecords` — the same workspace-wide set the metarecord list
// gathers and the bulk operations act on — so a selection can be gathered from
// the disk view too. Only rows that HAVE a metarecord can join it: an untracked
// entry has no uuid to check and says so instead of silently doing nothing.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/file-manager');

/** The shell's mount path: the panel's body markup into a Shadow root. */
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
type Listener = (value: unknown, key?: string) => void;

// The displayed directory /repo/music: dir1 and song.mp3 are tracked
// (u-dir1, u-song), plain.txt is not. The directory itself is u-music, its
// parent (the repo root) is u-root.
const ENTRIES = [
  { name: 'dir1', path: '/repo/music/dir1', is_dir: true },
  { name: 'song.mp3', path: '/repo/music/song.mp3', is_dir: false },
  { name: 'plain.txt', path: '/repo/music/plain.txt', is_dir: false },
];
const CHILDREN: Record<string, { uuid: string; name: string }[]> = {
  'u-music': [
    { uuid: 'u-dir1', name: 'dir1' },
    { uuid: 'u-song', name: 'song.mp3' },
  ],
  'u-root': [{ uuid: 'u-music', name: 'music' }],
};
const NODE_BY_PATH: Record<string, string> = {
  '/music': 'u-music',
  '': 'u-root',
};

function stub(vars: Record<string, unknown>, existing: Set<string>) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const store = new Map<string, unknown>([
    ['active_repo', 'r'],
    ['file-manager:start-dir', '/repo/music'],
    ...Object.entries(vars),
  ]);
  const listeners = new Map<string, Set<Listener>>();
  const writes: { key: string; value: unknown }[] = [];
  const notify = (key: string, value: unknown) => {
    for (const l of listeners.get(key) ?? []) l(value);
    for (const l of listeners.get('*') ?? []) l(value, key);
  };
  const fs = {
    readDir: vi.fn(async () => ENTRIES.map((e) => ({ ...e }))),
    stat: vi.fn(async () => ({ is_dir: false })),
    exists: vi.fn(async () => true),
    homeDir: vi.fn(async () => '/home/user'),
    mkdir: vi.fn(async () => {}),
    createFile: vi.fn(async () => {}),
    move: vi.fn(async () => {}),
    copy: vi.fn(async () => {}),
    remove: vi.fn(async () => {}),
  };
  const trash = {
    list: async () => [],
    restore: async () => '',
    remove: async () => {},
    empty: async () => 0,
    trashPath: async () => 'song.mp3',
  };
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
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
      call: async (method: string, path: string, body: unknown) => {
        const bare = path.split('?')[0];
        if (method === 'POST' && bare === '/repos/r/tree/resolve-path') {
          const rel = (body as { path?: string })?.path ?? '';
          return { uuid: NODE_BY_PATH[rel] ?? null };
        }
        if (method === 'GET' && bare === '/repos/r/tree/children') {
          const uuid = new URLSearchParams(path.split('?')[1] ?? '').get('uuid') ?? '';
          return CHILDREN[uuid] ?? [];
        }
        if (method === 'POST' && bare === '/repos/r/query') {
          // The pruning query: only the uuids that still exist come back.
          const query = (body as { query?: { type?: string; uuids?: string[] } })?.query;
          return { results: (query?.uuids ?? []).filter((u) => existing.has(u)) };
        }
        return { results: [] };
      },
      repoRoot: async () => '/repo',
      repoInternalDir: async () => '/repo/.metafolder/internal',
    },
    changes: { sync: vi.fn(async () => {}), subscribe: vi.fn(() => () => {}) },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => {
        store.set(key, value);
        writes.push({ key, value });
        notify(key, value); // the backend echoes every write to the writer too
      },
      adoptRepo: async () => {},
      onChange: (key: string, listener: Listener) => {
        let set = listeners.get(key);
        if (!set) {
          set = new Set();
          listeners.set(key, set);
        }
        set.add(listener);
      },
    },
    commands: {
      register: (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return Promise.resolve(null);
      },
      invoke: () => null,
    },
    fs,
    trash,
    statusBar,
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return {
    api,
    handlers,
    store,
    fs,
    statusBar,
    selected: () => (store.get('selected_metarecords') as string[] | undefined) ?? [],
    externalSet(key: string, value: unknown) {
      store.set(key, value);
      notify(key, value);
    },
  };
}

/** A few event-loop turns: open → enrich → render is several awaits deep. */
async function settle() {
  for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0));
}

async function mountFileManager(vars: Record<string, unknown> = {}, existing?: Set<string>) {
  const harness = stub(vars, existing ?? new Set(['u-root', 'u-music', 'u-dir1', 'u-song']));
  const shadow = shadowRoot();
  const mod = await import('../../default-config/panel-types/file-manager/main.js');
  await mod.mount(shadow, harness.api as never);
  await settle();
  return { ...harness, shadow };
}

/** The rendered rows, in order ("." , "..", then the directory's entries). */
function rows(shadow: ShadowRoot): Element[] {
  return [...shadow.querySelectorAll('#entries li')];
}

/** Moves the cursor onto `index` through the panel's own navigation (the
 *  cursor starts at -1, so row N takes N + 1 moves). */
async function moveTo(handlers: Map<string, Handler>, index: number) {
  for (let i = 0; i <= index; i++) await handlers.get('file-manager:next')!();
}

describe('checking metarecords from the file manager', () => {
  beforeEach(() => {
    vi.stubGlobal('confirm', vi.fn(() => true));
    // jsdom does not implement scrollIntoView, which select() calls.
    Element.prototype.scrollIntoView = () => {};
  });

  test('toggle checks then unchecks the tracked row under the cursor', async () => {
    const { handlers, selected, shadow } = await mountFileManager();
    await moveTo(handlers, 3); // song.mp3
    await handlers.get('file-manager:select')!('toggle');
    expect(selected()).toEqual(['u-song']);
    expect(rows(shadow)[3].classList.contains('checked')).toBe(true);
    await handlers.get('file-manager:select')!('toggle');
    expect(selected()).toEqual([]);
  });

  test('an untracked entry cannot be checked, and says so', async () => {
    const { handlers, selected, statusBar, store } = await mountFileManager();
    await moveTo(handlers, 4); // plain.txt
    await handlers.get('file-manager:select')!('toggle');
    expect(selected()).toEqual([]);
    expect(store.has('selected_metarecords')).toBe(false);
    expect(statusBar.message).toHaveBeenCalled();
  });

  test('select all checks the tracked entries, keeping what is already checked', async () => {
    const { handlers, selected } = await mountFileManager({ selected_metarecords: ['u-x'] }, new Set([
      'u-root',
      'u-music',
      'u-dir1',
      'u-song',
      'u-x',
    ]));
    await handlers.get('file-manager:select')!('all');
    // The directory's two tracked entries — not plain.txt (untracked), not the
    // synthetic "." / ".." rows (the directory and its parent).
    expect(selected()).toEqual(['u-x', 'u-dir1', 'u-song']);
  });

  test('select none clears the whole selection', async () => {
    const { handlers, selected } = await mountFileManager({ selected_metarecords: ['u-song'] });
    await handlers.get('file-manager:select')!('none');
    expect(selected()).toEqual([]);
  });

  test('checks made elsewhere (another list) light up the rows', async () => {
    const harness = await mountFileManager();
    harness.externalSet('selected_metarecords', ['u-song']);
    expect(rows(harness.shadow).map((row) => row.classList.contains('checked'))).toEqual([
      false,
      false,
      false,
      true,
      false,
    ]);
  });

  test('a metarecord that no longer exists is dropped when the listing reloads', async () => {
    const harness = await mountFileManager(
      { selected_metarecords: ['u-song', 'u-gone'] },
      new Set(['u-root', 'u-music', 'u-dir1', 'u-song']),
    );
    // The first listing already pruned the vanished one…
    expect(harness.selected()).toEqual(['u-song']);
    // …and another one checked elsewhere is pruned the same way on a reload.
    harness.externalSet('selected_metarecords', ['u-song', 'u-gone']);
    await harness.handlers.get('file-manager:refresh')!();
    expect(harness.selected()).toEqual(['u-song']);
  });
});
