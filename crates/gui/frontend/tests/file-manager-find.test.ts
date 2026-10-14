// file-manager "jump to an entry" picker (doc "file-manager panel"):
// `file-manager:find` collects the entry name in the command input, completing
// over the current directory's entries (filtered by ordered substring like every
// other picker), and moves the cursor onto the chosen entry.

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
type ArgSpec = { name: string; prompt: () => unknown; complete?: () => string[] | Promise<string[]> };
type Entry = { name: string; path: string; is_dir: boolean };

const DIRS: Record<string, Entry[]> = {
  '/repo': [
    { name: 'sub', path: '/repo/sub', is_dir: true },
    { name: 'notes.txt', path: '/repo/notes.txt', is_dir: false },
    { name: 'top.txt', path: '/repo/top.txt', is_dir: false },
    { name: '.hidden.txt', path: '/repo/.hidden.txt', is_dir: false },
  ],
  '/repo/sub': [{ name: 'song.mp3', path: '/repo/sub/song.mp3', is_dir: false }],
};

function stub() {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const specs = new Map<string, ArgSpec[]>();
  const vars: Record<string, unknown> = {};
  const fs = {
    readDir: vi.fn(async (path: string) => (DIRS[path] ?? []).map((e) => ({ ...e }))),
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
      repoRoot: async () => '/repo',
      repoInternalDir: async () => '/repo/.metafolder/internal',
    },
    changes: { sync: vi.fn(async () => {}), subscribe: vi.fn(() => () => {}) },
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? 'r' : (vars[key] ?? null)),
      set: vi.fn(async (key: string, value: unknown) => {
        vars[key] = value;
      }),
      onChange: noop,
    },
    commands: {
      register: (name: string, opts: { handler?: Handler; args?: ArgSpec[] }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        if (opts.args) specs.set(name, opts.args);
        return Promise.resolve(null);
      },
      invoke: vi.fn(() => null),
    },
    fs,
    statusBar,
    trash: { trashPath: vi.fn(async () => '') },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return { api, handlers, specs, fs, statusBar, root: shadowRoot() };
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

describe('file-manager:find', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {}; // jsdom has no scrollIntoView
  });

  test('completes over the current directory, directories marked with a slash', async () => {
    const s = stub();
    await mount(s);
    const args = s.specs.get('file-manager:find');
    expect(args).toHaveLength(2);
    expect(await args![1].complete!()).toEqual(['sub/', 'notes.txt', 'top.txt']);
  });

  test('an accepted completion moves the cursor onto that entry', async () => {
    const s = stub();
    await mount(s);
    await s.handlers.get('file-manager:find')!('select', 'top.txt');
    expect(cursorName(s.root)).toBe('top.txt');
    expect(s.api.workspace.set).toHaveBeenCalledWith('selected_paths', ['/repo/top.txt']);
  });

  test('a directory candidate selects it without descending', async () => {
    const s = stub();
    await mount(s);
    s.fs.readDir.mockClear();
    await s.handlers.get('file-manager:find')!('select', 'sub/');
    expect(cursorName(s.root)).toBe('sub');
    expect(s.fs.readDir).not.toHaveBeenCalled();
  });

  test('a typed value matches by ordered substring', async () => {
    const s = stub();
    await mount(s);
    await s.handlers.get('file-manager:find')!('select', 'no txt');
    expect(cursorName(s.root)).toBe('notes.txt');
  });

  test('no match reports an error and leaves the cursor alone', async () => {
    const s = stub();
    await mount(s);
    await s.handlers.get('file-manager:find')!('select', 'top.txt');
    await s.handlers.get('file-manager:find')!('select', 'zzz');
    expect(cursorName(s.root)).toBe('top.txt');
    expect(s.statusBar.error).toHaveBeenCalled();
  });

  test('hidden entries are offered only when hidden files are shown', async () => {
    const s = stub();
    await mount(s);
    const complete = s.specs.get('file-manager:find')![1].complete!;
    expect(await complete()).not.toContain('.hidden.txt');
    await s.handlers.get('file-manager:toggle')!('hidden');
    await new Promise((r) => setTimeout(r, 0));
    expect(await complete()).toContain('.hidden.txt');
  });

  test('walk mode enters a directory and asks again from inside it', async () => {
    const s = stub();
    await mount(s);
    await s.handlers.get('file-manager:find')!('walk', 'sub/');
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo/sub');
    expect(s.api.commands.invoke).toHaveBeenCalledWith('file-manager:find walk');
    expect(await s.specs.get('file-manager:find')![1].complete!()).toEqual(['song.mp3']);
  });

  test('walk mode selects a file and stops', async () => {
    const s = stub();
    await mount(s);
    await s.handlers.get('file-manager:find')!('walk', 'top.txt');
    expect(cursorName(s.root)).toBe('top.txt');
    expect(s.api.commands.invoke).not.toHaveBeenCalled();
  });
});
