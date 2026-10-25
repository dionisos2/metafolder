// `view:refresh file` (doc "Generic panel commands"): the panel is told when
// the selection changes, never when a file's content does — the refresh is
// what reads the file again, as it is on disk now.

import { describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/file');

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

type Provided = { refresh?: (mf?: unknown) => unknown };

function stub(path: string) {
  const noop = () => {};
  const vars = new Map<string, unknown>([
    ['selected_paths', [path]],
    ['selected_metarecord', null],
    ['active_repo', null],
  ]);
  const provided: Provided = {};
  const api = {
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'test-token',
    pageSize: 10,
    settings: {},
    defaults: {},
    whenVisible: (fn: () => void) => fn(),
    provide: (fns: Provided) => Object.assign(provided, fns),
    workspace: {
      get: async (key: string) => vars.get(key) ?? null,
      set: vi.fn(async () => {}),
      onChange: noop,
    },
    commands: { register: () => Promise.resolve(null), invoke: () => null },
    daemon: { call: async () => ({}), repoRoot: async () => '/' },
    fs: {
      stat: async () => ({ is_dir: false, kind: null }),
      exists: async () => true,
      readDir: async () => [],
    },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    recent: { touch: vi.fn(async () => {}) },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return { api, provided };
}

const flush = async () => {
  for (let i = 0; i < 6; i += 1) await new Promise((r) => setTimeout(r, 0));
};

describe('file panel — view:refresh', () => {
  test('reads the file again, as it is on disk now', async () => {
    let onDisk = 'before\n';
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({}),
        arrayBuffer: async () => new TextEncoder().encode(onDisk).buffer,
      })),
    );
    const root = shadowRoot();
    const { api, provided } = stub('/notes.txt');
    const mod = await import('../../default-config/panel-types/file/main.js');
    await mod.mount(root, api as never);
    await flush();
    const viewer = root.getElementById('viewer')!;
    expect(viewer.querySelector('pre')?.textContent).toBe('before\n');

    onDisk = 'after\n';
    expect(provided.refresh).toBeTypeOf('function');
    await provided.refresh!(api);
    await flush();
    expect(viewer.querySelector('pre')?.textContent).toBe('after\n');
  });
});
