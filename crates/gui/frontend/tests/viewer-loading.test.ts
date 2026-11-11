// While the next file is being read, the `viewer` panel must not go on showing
// the previous one under the new path: past a short grace it says it is
// loading (doc "viewer panel"). A demo screenshot once showed a photo's path
// over the text of the file selected before, the daemon being slow to answer.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { shippedDefaults, shippedSettings } from './support/shipped-defaults';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/viewer');

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

type Stat = { is_dir: boolean; kind: string | null };

/** A panel showing `/notes.txt`; `stat` answers for every other path. */
function stub(stat: (path: string) => Promise<Stat>) {
  const noop = () => {};
  const listeners = new Map<string, (value: unknown) => void>();
  const vars = new Map<string, unknown>([
    ['selected_paths', ['/notes.txt']],
    ['selected_metarecord', null],
    ['active_repo', null],
  ]);
  const api = {
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'test-token',
    pageSize: 10,
    settings: shippedSettings(),
    defaults: shippedDefaults(),
    whenVisible: (fn: () => void) => fn(),
    provide: () => {},
    workspace: {
      get: async (key: string) => vars.get(key) ?? null,
      set: vi.fn(async () => {}),
      onChange: (key: string, fn: (value: unknown) => void) => listeners.set(key, fn),
    },
    commands: { register: () => Promise.resolve(null), invoke: () => null },
    daemon: { call: async () => ({}), repoRoot: async () => '/' },
    fs: {
      stat: async (path: unknown) =>
        path === '/notes.txt' ? { is_dir: false, kind: null } : stat(String(path)),
      exists: async () => true,
      readDir: async () => [],
    },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    recent: { touch: vi.fn(async () => {}) },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  const select = (path: string) => {
    vars.set('selected_paths', [path]);
    listeners.get('selected_paths')?.([path]);
  };
  return { api, select };
}

const TEXT = 'the previous file\n';

describe('viewer panel — the file being loaded', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({}),
        arrayBuffer: async () => new TextEncoder().encode(TEXT).buffer,
      })),
    );
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  async function mounted(stat: (path: string) => Promise<Stat>) {
    const root = shadowRoot();
    const { api, select } = stub(stat);
    const mod = await import('../../default-config/panel-types/viewer/main.js');
    await mod.mount(root, api as never);
    await vi.advanceTimersByTimeAsync(0);
    const viewer = root.getElementById('viewer')!;
    expect(viewer.querySelector('pre')?.textContent).toContain('the previous file');
    return { viewer, select };
  }

  test('a slow read replaces the previous file by a loading note, then the new file', async () => {
    let answer: (stat: Stat) => void = () => {};
    const { viewer, select } = await mounted(
      () => new Promise<Stat>((resolve) => (answer = resolve)),
    );
    select('/photo.jpg');
    await vi.advanceTimersByTimeAsync(1000);
    expect(viewer.querySelector('pre')).toBeNull();
    expect(viewer.querySelector('.placeholder')?.textContent).toBe('Loading…');

    answer({ is_dir: false, kind: 'image' });
    await vi.advanceTimersByTimeAsync(0);
    expect(viewer.querySelector('img')?.getAttribute('src')).toContain('photo.jpg');
  });

  test('a quick read never shows the loading note', async () => {
    const { viewer, select } = await mounted(async () => ({ is_dir: false, kind: 'image' }));
    select('/photo.jpg');
    await vi.advanceTimersByTimeAsync(1000);
    expect(viewer.querySelector('.placeholder')).toBeNull();
    expect(viewer.querySelector('img')?.getAttribute('src')).toContain('photo.jpg');
  });
});
