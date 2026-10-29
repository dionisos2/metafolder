// The file manager joins the checked multi-selection (doc "file-manager panel",
// doc "Cross-panel selection"): a tracked row can be checked into
// `selected_metarecords` — the same workspace-wide set the metarecord list
// gathers and the bulk operations act on — so a selection can be gathered from
// the disk view too. An untracked entry has no uuid: it is checked by its path,
// into `selected_untracked_paths`, which the file operations take too.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { cursor, generic, provided } from './support/cursor';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { withRuns } from './support/runs';

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

/** When set, the tracked-children read waits for it (a slow daemon). */
let childrenGate: Promise<void> | null = null;
/** Paths no longer on disk (`fs.exists` says false). */
const gone = new Set<string>();

function stub(vars: Record<string, unknown>, existing: Set<string>) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const store = new Map<string, unknown>([
    ['active_repo', 'r'],
    ['file-manager:dir', '/repo/music'],
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
    exists: vi.fn(async (path: string) => !gone.has(path)),
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
          if (childrenGate) await childrenGate;
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
    provide: vi.fn(),
    commands: {
      register: (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return Promise.resolve(null);
      },
      invoke: () => null,
    },
    fs,
    trash,
    recent: { touch: vi.fn(async () => {}), list: vi.fn(async () => []) },
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
    untracked: () => (store.get('selected_untracked_paths') as string[] | undefined) ?? [],
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
  await mod.mount(shadow, withRuns(harness.api) as never);
  await settle();
  return { ...harness, shadow };
}

/** The rendered rows, in order ("." , "..", then the directory's entries). */
function rows(shadow: ShadowRoot): Element[] {
  return [...shadow.querySelectorAll('#entries li')];
}

/** Moves the cursor onto `index` through the panel's own navigation (the
 *  cursor starts at -1, so row N takes N + 1 moves). */
async function moveTo(api: { provide: Parameters<typeof cursor>[0] }, index: number) {
  for (let i = 0; i <= index; i++) await cursor(api.provide, 'next');
}

describe('checking metarecords from the file manager', () => {
  beforeEach(() => {
    vi.stubGlobal('confirm', vi.fn(() => true));
    // jsdom does not implement scrollIntoView, which select() calls.
    Element.prototype.scrollIntoView = () => {};
  });

  test('toggle checks then unchecks the tracked row under the cursor', async () => {
    const { api, handlers, selected, shadow } = await mountFileManager();
    await moveTo(api, 3); // song.mp3
    await handlers.get('file-manager:select')!('toggle');
    expect(selected()).toEqual(['u-song']);
    expect(rows(shadow)[3].classList.contains('checked')).toBe(true);
    await handlers.get('file-manager:select')!('toggle');
    expect(selected()).toEqual([]);
  });

  test('an untracked entry is checked by its path', async () => {
    const { api, handlers, selected, untracked, shadow } = await mountFileManager();
    await moveTo(api, 4); // plain.txt
    await handlers.get('file-manager:select')!('toggle');
    expect(selected()).toEqual([]);
    expect(untracked()).toEqual(['/repo/music/plain.txt']);
    expect(rows(shadow)[4].classList.contains('checked')).toBe(true);
    expect(shadow.getElementById('status-line')!.textContent).toContain('1 selected');
    await handlers.get('file-manager:select')!('toggle');
    expect(untracked()).toEqual([]);
  });

  test('without a repository, entries are checked by their paths', async () => {
    const { api, handlers, untracked } = await mountFileManager({ active_repo: null });
    await moveTo(api, 3); // song.mp3, untracked here
    await handlers.get('file-manager:select')!('toggle');
    expect(untracked()).toEqual(['/repo/music/song.mp3']);
  });

  test('select all checks every entry, keeping what is already checked', async () => {
    const { handlers, selected, untracked } = await mountFileManager({ selected_metarecords: ['u-x'] }, new Set([
      'u-root',
      'u-music',
      'u-dir1',
      'u-song',
      'u-x',
    ]));
    await handlers.get('file-manager:select')!('all');
    // The directory's entries — the tracked by their metarecords, plain.txt by
    // its path — not the synthetic "." / ".." rows (the directory and its parent).
    expect(selected()).toEqual(['u-x', 'u-dir1', 'u-song']);
    expect(untracked()).toEqual(['/repo/music/plain.txt']);
  });

  test('select none clears the whole selection', async () => {
    const { handlers, selected, untracked } = await mountFileManager({
      selected_metarecords: ['u-song'],
      selected_untracked_paths: ['/repo/music/plain.txt'],
    });
    await handlers.get('file-manager:select')!('none');
    expect(selected()).toEqual([]);
    expect(untracked()).toEqual([]);
  });

  test('a checked path lights its row, wherever it was checked', async () => {
    const harness = await mountFileManager();
    harness.externalSet('selected_untracked_paths', ['/repo/music/plain.txt']);
    expect(rows(harness.shadow)[4].classList.contains('checked')).toBe(true);
  });

  test('a checked path recorded since moves to the checked metarecords', async () => {
    const harness = await mountFileManager(
      { selected_untracked_paths: ['/repo/music/plain.txt'] },
      new Set(['u-root', 'u-music', 'u-dir1', 'u-song', 'u-plain']),
    );
    CHILDREN['u-music'].push({ uuid: 'u-plain', name: 'plain.txt' });
    try {
      await generic(harness.api.provide, 'view:refresh');
      await settle();
      expect(harness.untracked()).toEqual([]);
      expect(harness.selected()).toEqual(['u-plain']);
    } finally {
      CHILDREN['u-music'].pop();
    }
  });

  test('a checked path no longer on disk is dropped when the listing reloads', async () => {
    gone.add('/elsewhere/gone.txt');
    try {
      const harness = await mountFileManager({
        selected_untracked_paths: ['/elsewhere/kept.txt', '/elsewhere/gone.txt'],
      });
      expect(harness.untracked()).toEqual(['/elsewhere/kept.txt']);
    } finally {
      gone.clear();
    }
  });

  test('the file operations take the checked paths; recording, only the untracked', async () => {
    const harness = await mountFileManager({
      selected_untracked_paths: ['/repo/music/plain.txt'],
    });
    const fns = provided(harness.api.provide as never);
    expect(await fns.paths!()).toEqual(['/repo/music/plain.txt']);
    expect(await fns.recordable!()).toEqual(['/repo/music/plain.txt']);
    // Nothing checked: the entry under the cursor, as before.
    await harness.handlers.get('file-manager:select')!('none');
    await moveTo(harness.api, 3); // song.mp3
    expect(await fns.paths!()).toEqual(['/repo/music/song.mp3']);
    expect(await fns.recordable!()).toEqual(['/repo/music/song.mp3']);
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
    await generic(harness.api.provide, 'view:refresh');
    expect(harness.selected()).toEqual(['u-song']);
  });
});

// The tracked status arrives after the rows (doc "file-manager panel"): a
// cursor put on an entry before it did published the path without its
// metarecord, and the selection stayed "untracked" until the cursor moved —
// so Shift+Enter refused to show a tracked file's metarecord (doc "Opening a row").
describe('the selection when the tracked status arrives late', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
  });

  test("the entry under the cursor gets its metarecord once it is known", async () => {
    let release = () => {};
    childrenGate = new Promise<void>((r) => (release = r));
    try {
      const harness = stub({}, new Set(['u-root', 'u-music', 'u-dir1', 'u-song']));
      const shadow = shadowRoot();
      const mod = await import('../../default-config/panel-types/file-manager/main.js');
      await mod.mount(shadow, withRuns(harness.api) as never);
      await settle();
      await moveTo(harness.api, 3); // song.mp3, its tracked status not read yet
      expect(harness.store.get('selected_metarecord')).toBeNull();
      release();
      await settle();
      expect(harness.store.get('selected_metarecord')).toEqual({ uuid: 'u-song', repo: 'r' });
      expect(harness.store.get('selected_paths')).toEqual(['/repo/music/song.mp3']);
    } finally {
      childrenGate = null;
    }
  });

  test('a selection made elsewhere meanwhile is left alone', async () => {
    let release = () => {};
    childrenGate = new Promise<void>((r) => (release = r));
    try {
      const harness = stub({}, new Set(['u-root', 'u-music', 'u-dir1', 'u-song']));
      const shadow = shadowRoot();
      const mod = await import('../../default-config/panel-types/file-manager/main.js');
      await mod.mount(shadow, withRuns(harness.api) as never);
      await settle();
      await moveTo(harness.api, 3);
      // Another panel selects something else before the read lands.
      harness.externalSet('selected_metarecord', { uuid: 'u-other', repo: 'r' });
      harness.externalSet('selected_paths', ['/elsewhere/x']);
      release();
      await settle();
      expect(harness.store.get('selected_metarecord')).toEqual({ uuid: 'u-other', repo: 'r' });
      expect(harness.store.get('selected_paths')).toEqual(['/elsewhere/x']);
    } finally {
      childrenGate = null;
    }
  });
});
