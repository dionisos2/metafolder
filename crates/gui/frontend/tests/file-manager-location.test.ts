// file-manager follows its location variables (doc "Cross-panel selection"):
// `file-manager:dir` (the folder shown) and `file-manager:cursor` (the name of
// the entry highlighted in it) ARE its location. A command shows a folder by
// writing them — `file-manager:reveal` does, for the selection's folder — and
// the panel goes there, at mount and while mounted; what it compares a write
// with is the location it last showed or wrote, so its own writes coming back
// change nothing.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import shipped from '../../default-config/commands.js';

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
type Entry = { name: string; path: string; is_dir: boolean };

/** Per-directory listings, keyed by absolute path. Directories not listed here
 *  come back empty. */
const DIRS: Record<string, Entry[]> = {
  '/repo': [
    { name: 'sub', path: '/repo/sub', is_dir: true },
    { name: 'top.txt', path: '/repo/top.txt', is_dir: false },
  ],
  '/repo/sub': [{ name: 'song.mp3', path: '/repo/sub/song.mp3', is_dir: false }],
  '/elsewhere': [{ name: 'far.txt', path: '/elsewhere/far.txt', is_dir: false }],
};

function stub(repo: string | null, vars: Record<string, unknown>, repoRoot = '/repo') {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const subscriptions = new Map<string, (value: unknown) => void>();
  const fs = {
    readDir: vi.fn(async (path: string) => (DIRS[path] ?? []).map((e) => ({ ...e }))),
    // A path is a directory iff it is one of the listed directory keys.
    stat: vi.fn(async (path: string) => ({ is_dir: path in DIRS })),
    exists: vi.fn(async () => true),
    homeDir: vi.fn(async () => '/home/user'),
    mkdir: vi.fn(async () => {}),
    createFile: vi.fn(async () => {}),
    move: vi.fn(async () => {}),
    copy: vi.fn(async () => {}),
    remove: vi.fn(async () => {}),
  };
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const setVar = vi.fn(async (key: string, value: unknown) => {
    vars[key] = value;
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
      call: async () => ({ results: [], next_cursor: null }),
      repoRoot: async () => repoRoot,
      repoInternalDir: async () => `${repoRoot}/.metafolder/internal`,
    },
    changes: { sync: vi.fn(async () => {}), subscribe: vi.fn(() => () => {}) },
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? repo : (vars[key] ?? null)),
      set: setVar,
      onChange: (key: string, cb: (value: unknown) => void) => subscriptions.set(key, cb),
    },
    commands: {
      register: (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return Promise.resolve(null);
      },
      invoke: () => null,
    },
    fs,
    statusBar,
    trash: { trashPath: vi.fn(async () => '') },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return { api, handlers, fs, statusBar, setVar, subscriptions, root: shadowRoot() };
}

async function mount(s: ReturnType<typeof stub>) {
  const mod = await import('../../default-config/panel-types/file-manager/main.js');
  await mod.mount(s.root, s.api as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
}

/** The name of the cursor-highlighted entry, or null. */
function cursorName(root: ShadowRoot): string | null {
  return root.querySelector('li.cursor .name')?.textContent ?? null;
}

/** Lets a coalesced variable write and the listing it starts land. */
async function settle() {
  for (let i = 0; i < 5; i += 1) await new Promise((r) => setTimeout(r, 0));
}

/** Writes variables as a command does, each pushed to the panel. */
async function push(s: ReturnType<typeof stub>, vars: Record<string, unknown>) {
  for (const [key, value] of Object.entries(vars)) {
    await s.api.workspace.set(key, value);
    s.subscriptions.get(key)?.(value);
  }
  await settle();
}

describe('file-manager location variables', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {}; // jsdom has no scrollIntoView
  });

  test('a location present at mount is opened, its entry highlighted', async () => {
    const s = stub('r', { 'file-manager:dir': '/repo/sub', 'file-manager:cursor': 'song.mp3' });
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo/sub');
    expect(s.fs.readDir).not.toHaveBeenCalledWith('/repo');
    expect(s.setVar).toHaveBeenCalledWith('selected_paths', ['/repo/sub/song.mp3']);
    expect(cursorName(s.root)).toBe('song.mp3');
  });

  test('a folder alone is opened with nothing highlighted', async () => {
    const s = stub('r', { 'file-manager:dir': '/repo/sub' });
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo/sub');
    expect(s.setVar).not.toHaveBeenCalledWith('selected_paths', ['/repo/sub/song.mp3']);
  });

  test('no location falls back to the repo root', async () => {
    const s = stub('r', {});
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo');
    expect(s.fs.readDir).not.toHaveBeenCalledWith('/repo/sub');
  });

  test('a location written while mounted is gone to', async () => {
    const s = stub('r', {});
    await mount(s);
    s.fs.readDir.mockClear();
    await push(s, { 'file-manager:dir': '/repo/sub', 'file-manager:cursor': 'song.mp3' });
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo/sub');
    expect(cursorName(s.root)).toBe('song.mp3');
  });

  test('the location it already shows is not listed again', async () => {
    const s = stub('r', { 'file-manager:dir': '/repo/sub', 'file-manager:cursor': 'song.mp3' });
    await mount(s);
    s.fs.readDir.mockClear();
    await push(s, { 'file-manager:dir': '/repo/sub', 'file-manager:cursor': 'song.mp3' });
    expect(s.fs.readDir).not.toHaveBeenCalled();
  });

  test('navigating writes the location, and its own writes coming back change nothing', async () => {
    const s = stub('r', {});
    await mount(s);
    await s.handlers.get('file-manager:find')!('sub');
    // The cursor is on `sub`: that is the location now.
    expect(s.setVar).toHaveBeenCalledWith('file-manager:cursor', 'sub');
    s.fs.readDir.mockClear();
    s.setVar.mockClear();
    // The shell pushes every write back to the panel that made it.
    s.subscriptions.get('file-manager:dir')?.('/repo');
    s.subscriptions.get('file-manager:cursor')?.('sub');
    await settle();
    expect(s.fs.readDir).not.toHaveBeenCalled();
    expect(cursorName(s.root)).toBe('sub');
  });

  test('opening a folder resets the cursor variable', async () => {
    const s = stub('r', { 'file-manager:dir': '/repo', 'file-manager:cursor': 'top.txt' });
    await mount(s);
    s.setVar.mockClear();
    await push(s, { 'file-manager:dir': '/repo/sub', 'file-manager:cursor': null });
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo/sub');
    expect(cursorName(s.root)).toBe(null);
  });

  test('a location outside the repo root drops the constraint to go there', async () => {
    const s = stub('r', { 'file-manager:dir': '/elsewhere', 'file-manager:cursor': 'far.txt' });
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/elsewhere');
    expect(s.root.getElementById('constrain')).toHaveProperty('checked', false);
    expect(cursorName(s.root)).toBe('far.txt');
  });
});

// ── The command side, as the shipped `commands.js` defines it ───────────────
// `file-manager:reveal` resolves the selection's first path to a folder and the
// entry to highlight in it — statting it to tell a folder from a file — writes
// them as the panel's location, and switches the focused slot.

describe('the file-manager:reveal command (shipped commands.js)', () => {
  const calls = {
    sets: [] as { key: string; value: unknown }[],
    invoked: [] as string[],
    status: [] as string[],
  };
  const state = { paths: null as unknown, dirs: ['/repo/sub'] as string[] };

  function fakeMf() {
    return {
      workspace: {
        get: async (key: string) => (key === 'selected_paths' ? state.paths : null),
        set: async (key: string, value: unknown) => {
          calls.sets.push({ key, value });
        },
      },
      fs: {
        stat: async (path: string) => {
          if (path.includes('gone')) throw new Error('no such file');
          return { is_dir: state.dirs.includes(path) };
        },
      },
      invoke: (invocation: string) => {
        calls.invoked.push(invocation);
        return Promise.resolve({ ok: true });
      },
      statusBar: {
        message: async () => {},
        error: async (error: unknown) => {
          calls.status.push(String(error));
        },
      },
    };
  }

  beforeEach(() => {
    calls.sets.length = 0;
    calls.invoked.length = 0;
    calls.status.length = 0;
    state.paths = ['/repo/sub/song.mp3'];
  });

  test('no selection: says so, and switches nothing', async () => {
    state.paths = null;

    await shipped['file-manager:reveal'].run(fakeMf() as never);

    expect(calls.status).toEqual(['no file or folder is selected']);
    expect(calls.sets).toEqual([]);
    expect(calls.invoked).toEqual([]);
  });

  test('a file: its folder, with the file highlighted, then the panel switches', async () => {
    await shipped['file-manager:reveal'].run(fakeMf() as never);

    expect(calls.sets).toEqual([
      { key: 'file-manager:dir', value: '/repo/sub' },
      { key: 'file-manager:cursor', value: 'song.mp3' },
    ]);
    expect(calls.invoked).toEqual(['panel:set type file-manager']);
  });

  test('a folder: the folder itself, nothing highlighted', async () => {
    state.paths = ['/repo/sub'];

    await shipped['file-manager:reveal'].run(fakeMf() as never);

    expect(calls.sets).toEqual([
      { key: 'file-manager:dir', value: '/repo/sub' },
      { key: 'file-manager:cursor', value: null },
    ]);
  });

  test('a path that is gone is taken for a file: its folder opens', async () => {
    state.paths = ['/repo/gone/old.txt'];

    await shipped['file-manager:reveal'].run(fakeMf() as never);

    expect(calls.sets).toEqual([
      { key: 'file-manager:dir', value: '/repo/gone' },
      { key: 'file-manager:cursor', value: 'old.txt' },
    ]);
  });

  test('the first *string* entry is taken (the selection carries metadata too)', async () => {
    state.paths = [42, '/repo/top.txt', '/repo/sub/song.mp3'];

    await shipped['file-manager:reveal'].run(fakeMf() as never);

    expect(calls.sets[0]).toEqual({ key: 'file-manager:dir', value: '/repo' });
  });
});
