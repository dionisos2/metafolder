// What the `file` panel shows follows what the file *is* — the kind the GUI
// reads from its first bytes (`fs.stat().kind`, doc "GUI file endpoints") —
// and not its extension: `.ts` is an MPEG transport stream or a TypeScript
// source, and only the content says which.

import { beforeEach, describe, expect, test, vi } from 'vitest';
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

function stub(path: string, kind: string | null) {
  const noop = () => {};
  const vars = new Map<string, unknown>([
    ['selected_paths', [path]],
    ['selected_metarecord', null],
    ['active_repo', null],
  ]);
  return {
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'test-token',
    pageSize: 10,
    settings: {},
    defaults: {},
    whenVisible: (fn: () => void) => fn(),
    workspace: {
      get: async (key: string) => vars.get(key) ?? null,
      set: vi.fn(async () => {}),
      onChange: noop,
    },
    commands: { register: () => Promise.resolve(null), invoke: () => null },
    daemon: { call: async () => ({}), repoRoot: async () => '/' },
    fs: {
      stat: async () => ({ is_dir: false, kind }),
      exists: async () => true,
      readDir: async () => [],
    },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    recent: { touch: vi.fn(async () => {}) },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
}

const flush = async () => {
  for (let i = 0; i < 6; i += 1) await new Promise((r) => setTimeout(r, 0));
};

const SOURCE = 'export const answer: number = 42;\n';

describe('file panel — the kind comes from the content', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes('/document/info')) {
          return { ok: true, status: 200, json: async () => ({ pages: 2 }) };
        }
        return {
          ok: true,
          status: 200,
          json: async () => ({}),
          arrayBuffer: async () => new TextEncoder().encode(SOURCE).buffer,
        };
      }),
    );
  });

  async function show(path: string, kind: string | null) {
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file/main.js');
    await mod.mount(root, stub(path, kind) as never);
    await flush();
    return root.getElementById('viewer')!;
  }

  test('a TypeScript source named .ts is shown as text, not as a video', async () => {
    const viewer = await show('/src/main.ts', null);
    expect(viewer.querySelector('.media-poster')).toBeNull();
    expect(viewer.querySelector('pre')?.textContent).toContain('export const answer');
  });

  test('a transport stream named .ts is a video', async () => {
    const viewer = await show('/films/clip.ts', 'video');
    expect(viewer.querySelector('.media-poster')).not.toBeNull();
  });

  test('a video is a video whatever its name', async () => {
    const viewer = await show('/films/holiday.dat', 'video');
    expect(viewer.querySelector('.media-poster')).not.toBeNull();
  });

  test('a PDF without the extension is paged like any other', async () => {
    const viewer = await show('/docs/report', 'document');
    expect(viewer.querySelector('img')?.getAttribute('src')).toContain('/document?path=');
  });

  test('an image without the extension is shown as an image', async () => {
    const viewer = await show('/photos/scan', 'image');
    expect(viewer.querySelector('img')?.getAttribute('src')).toContain('/fsraw?path=');
  });

  test('a name that says image over content that is not one is not an <img>', async () => {
    const viewer = await show('/photos/notes.png', null);
    expect(viewer.querySelector('img')).toBeNull();
  });
});
