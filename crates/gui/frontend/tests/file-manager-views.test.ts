// A view is counted only when the user chooses (doc "Why recently viewed is
// kept by the GUI"): the entry the file manager highlights by itself — the one
// its saved location names, the folder it comes back up from — is published
// marked `auto`, and its first listing, at mount, is not a visit of the folder.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { withRuns } from './support/runs';
import { provided } from './support/cursor';
import { shippedDefaults } from './support/shipped-defaults';

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
  '/repo/sub': [
    { name: 'song.mp3', path: '/repo/sub/song.mp3', is_dir: false },
    { name: 'deep', path: '/repo/sub/deep', is_dir: true },
  ],
  '/repo/sub/deep': [],
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
    defaults: shippedDefaults(),
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => void) => fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      call: vi.fn(async (method: string, path: string, req?: { path?: string }) => {
        // A folder's metarecord is `u:` and its absolute path.
        if (path.includes('/tree/resolve-path')) {
          const abs = `${repoRoot}/${req?.path ?? ''}`.replace(/\/+/g, '/').replace(/(.)\/$/, '$1');
          return { uuid: `u:${abs}` };
        }
        if (path.endsWith('/watch')) return { quiet_period_ms: 3000 };
        if (path.includes('/tree/children')) {
          const dir = (new URLSearchParams(path.split('?')[1]).get('uuid') ?? '').slice(2);
          return (DIRS[dir] ?? []).map((e) => ({ uuid: `u:${e.path}`, name: e.name }));
        }
        return { results: [], next_cursor: null };
      }),
      repoRoot: async () => repoRoot,
      repoInternalDir: async () => `${repoRoot}/.metafolder/internal`,
    },
    changes: { sync: vi.fn(async () => {}), subscribe: vi.fn(() => () => {}) },
    workspace: {
      mayMoveSelection: () => true,
      get: async (key: string) => (key === 'active_repo' ? repo : (vars[key] ?? null)),
      set: setVar,
      onChange: (key: string, cb: (value: unknown) => void) => subscriptions.set(key, cb),
    },
    provide: vi.fn(),
    commands: {
      register: (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return Promise.resolve(null);
      },
      invoke: vi.fn(async (_line: string) => null),
    },
    fs,
    recent: { touch: vi.fn(async () => {}), list: vi.fn(async () => []) },
    statusBar,
    trash: { trashPath: vi.fn(async () => '') },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return { api, handlers, fs, statusBar, setVar, subscriptions, vars, root: shadowRoot() };
}

async function mount(s: ReturnType<typeof stub>) {
  const mod = await import('../../default-config/panel-types/file-manager/main.js');
  await mod.mount(s.root, withRuns(s.api) as never);
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

/** The row showing `name`. */
function rowOf(root: ShadowRoot, name: string): HTMLLIElement {
  return [...root.querySelectorAll('li')].find(
    (li) => li.querySelector('.name')?.textContent === name,
  )!;
}


describe('file-manager views', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
  });

  test('the entry its saved location highlights is published marked auto', async () => {
    const s = stub('r', { 'file-manager:dir': '/repo', 'file-manager:cursor': 'top.txt' });
    await mount(s);
    await settle();
    expect(cursorName(s.root)).toBe('top.txt');
    expect(s.setVar).toHaveBeenCalledWith('selected_metarecord', {
      uuid: 'u:/repo/top.txt',
      repo: 'r',
      auto: true,
    });
    expect(s.setVar).not.toHaveBeenCalledWith('selected_metarecord', {
      uuid: 'u:/repo/top.txt',
      repo: 'r',
    });
  });

  test('a click publishes the entry unmarked', async () => {
    const s = stub('r', { 'file-manager:dir': '/repo', 'file-manager:cursor': 'top.txt' });
    await mount(s);
    await settle();
    rowOf(s.root, 'top.txt').click();
    await settle();
    expect(s.vars.selected_metarecord).toEqual({ uuid: 'u:/repo/top.txt', repo: 'r' });
  });

  test('the folder it comes back up from is highlighted marked auto', async () => {
    const s = stub('r', { 'file-manager:dir': '/repo/sub' });
    await mount(s);
    await settle();
    await (provided(s.api.provide as never) as { parent: () => Promise<void> }).parent();
    await settle();
    expect(cursorName(s.root)).toBe('sub');
    expect(s.vars.selected_metarecord).toEqual({ uuid: 'u:/repo/sub', repo: 'r', auto: true });
  });

  test('the first listing is not a visit of the folder; going into one is', async () => {
    const s = stub('r', {});
    await mount(s);
    await settle();
    expect(s.api.recent.touch).not.toHaveBeenCalled();
    rowOf(s.root, 'sub').querySelector<HTMLButtonElement>('button.mf-open')!.click();
    await settle();
    expect(s.api.recent.touch).toHaveBeenCalledTimes(1);
    expect(s.api.recent.touch).toHaveBeenCalledWith('r', 'u:/repo/sub');
  });
});
