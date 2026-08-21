// file-manager "reveal a folder" flow (spec-gui "Cross-panel selection"): the
// panel honours a `file-manager:reveal-path` workspace variable — set by the
// `file-manager:reveal-folder` command from another panel — navigating to the
// metarecord's folder (its parent for a file, highlighting the file), both when
// it mounts and while already mounted, guarded by the request nonce.

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
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => void) => fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      call: async () => ({ results: [], next_cursor: null }),
      repoRoot: async () => repoRoot,
      repoInternalDir: async () => `${repoRoot}/.metafolder/internal`,
    },
    cache: { sync: vi.fn(async () => {}) },
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

describe('file-manager reveal-folder', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {}; // jsdom has no scrollIntoView
  });

  test('a file request opens its containing folder and highlights the file', async () => {
    const s = stub('r', { 'file-manager:reveal-path': { path: '/repo/sub/song.mp3', nonce: 1 } });
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo/sub');
    expect(s.setVar).toHaveBeenCalledWith('selected_paths', ['/repo/sub/song.mp3']);
    expect(cursorName(s.root)).toBe('song.mp3');
  });

  test('a directory request opens the folder itself, nothing highlighted', async () => {
    const s = stub('r', { 'file-manager:reveal-path': { path: '/repo/sub', nonce: 1 } });
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo/sub');
    // The current directory is the folder itself; no file is highlighted.
    expect(s.setVar).not.toHaveBeenCalledWith('selected_paths', ['/repo/sub/song.mp3']);
  });

  test('no reveal request falls back to the repo root', async () => {
    const s = stub('r', {});
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo');
    expect(s.fs.readDir).not.toHaveBeenCalledWith('/repo/sub');
  });

  test('a request while already mounted navigates on the onChange', async () => {
    const s = stub('r', {}); // no request at mount
    await mount(s);
    s.fs.readDir.mockClear();
    // The command sets the variable, then the panel's onChange fires.
    s.api.workspace.set('file-manager:reveal-path', { path: '/repo/sub/song.mp3', nonce: 7 });
    s.subscriptions.get('file-manager:reveal-path')!({ path: '/repo/sub/song.mp3', nonce: 7 });
    await new Promise((r) => setTimeout(r, 0));
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo/sub');
    expect(cursorName(s.root)).toBe('song.mp3');
  });

  test('the same nonce is acted on only once', async () => {
    const s = stub('r', { 'file-manager:reveal-path': { path: '/repo/sub/song.mp3', nonce: 1 } });
    await mount(s); // start() handles nonce 1
    s.fs.readDir.mockClear();
    // A second delivery of the identical request (same nonce) does nothing.
    s.subscriptions.get('file-manager:reveal-path')!({ path: '/repo/sub/song.mp3', nonce: 1 });
    await new Promise((r) => setTimeout(r, 0));
    expect(s.fs.readDir).not.toHaveBeenCalled();
  });

  test('a target outside the repo root drops the constraint to navigate there', async () => {
    const s = stub('r', { 'file-manager:reveal-path': { path: '/elsewhere/far.txt', nonce: 1 } });
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/elsewhere');
    expect(s.root.getElementById('constrain')).toHaveProperty('checked', false);
    expect(cursorName(s.root)).toBe('far.txt');
  });

  test('a malformed request is ignored (falls back to the root)', async () => {
    const s = stub('r', { 'file-manager:reveal-path': { nonce: 1 } }); // no path
    await mount(s);
    expect(s.fs.readDir).toHaveBeenCalledWith('/repo');
    expect(s.fs.readDir).not.toHaveBeenCalledWith('/repo/sub');
  });
});
