// The selection is a PAIR: `selected_metarecord` and `selected_paths` name the
// same thing, and a publisher sets the METARECORD FIRST. Consumers react to
// `selected_paths` and read `selected_metarecord` right then — the `file`
// panel does, to learn which metarecord holds the playback position of the
// file it is about to show — so publishing the paths first hands them the
// PREVIOUS metarecord: one file on screen, another one written to.
//
// This pins the order for the file-manager, the one panel that had it
// backwards. (The scripting API's `view file --path`, the other publisher that
// had it backwards, is pinned in crates/gui/tests/gui_api.rs.)

import { describe, expect, test, vi, beforeEach } from 'vitest';
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

type Entry = { name: string; path: string; is_dir: boolean };

const DIRS: Record<string, Entry[]> = {
  '/repo': [
    { name: 'a.txt', path: '/repo/a.txt', is_dir: false },
    { name: 'b.txt', path: '/repo/b.txt', is_dir: false },
  ],
};

function stub() {
  const noop = () => {};
  const vars: Record<string, unknown> = {};
  const subscriptions = new Map<string, (value: unknown) => void>();
  const setVar = vi.fn(async (key: string, value: unknown) => {
    vars[key] = value;
  });
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
    cache: { sync: vi.fn(async () => {}), subscribe: vi.fn(() => () => {}) },
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? 'r' : (vars[key] ?? null)),
      set: setVar,
      onChange: (key: string, cb: (value: unknown) => void) => subscriptions.set(key, cb),
    },
    commands: {
      register: () => Promise.resolve(null),
      invoke: () => null,
    },
    fs,
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    trash: { trashPath: vi.fn(async () => '') },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return { api, setVar, root: shadowRoot() };
}

describe('file-manager selection pair', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {}; // jsdom has no scrollIntoView
  });

  test('publishes selected_metarecord before selected_paths', async () => {
    const s = stub();
    const mod = await import('../../default-config/panel-types/file-manager/main.js');
    await mod.mount(s.root, s.api as never);
    await new Promise((r) => setTimeout(r, 0));

    // Move the cursor: that is what propagates the selection.
    const rows = s.root.querySelectorAll('li');
    (rows[1] as HTMLElement).dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await new Promise((r) => setTimeout(r, 0));

    const keys = s.setVar.mock.calls
      .map(([key]) => key)
      .filter((key) => key === 'selected_metarecord' || key === 'selected_paths');
    expect(keys.length).toBeGreaterThan(0);
    // Every propagation, in order: the metarecord, then the paths.
    for (let i = 0; i + 1 < keys.length; i += 2) {
      expect([keys[i], keys[i + 1]]).toEqual(['selected_metarecord', 'selected_paths']);
    }
  });
});
