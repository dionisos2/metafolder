// The `file` panel's right-click menu acts on the file the panel is *showing*
// (doc "file panel"). After drilling into a folder's listing — or stepping up
// with `file:back` — that is no longer the selected path, and a menu still
// aimed at the selection would rename or trash something that is not on screen.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { getClipboard, setClipboard } from '../../panel-shim/file-actions.js';

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

type Handler = (...args: string[]) => unknown;
type MenuItem = { label?: string; action?: () => void };
type Provider = (event: MouseEvent) => MenuItem[];

const DIRS = new Set(['/repo/album']);

function stub(path: string, tracked = true) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const providers: Provider[] = [];
  const vars = new Map<string, unknown>([
    ['selected_paths', [path]],
    ['selected_metarecord', tracked ? { uuid: 'u1', repo: 'r1' } : null],
    ['active_repo', 'r1'],
  ]);
  return {
    handlers,
    providers,
    api: {
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
      commands: {
        register: (name: string, opts: { handler?: Handler }) => {
          if (opts.handler) handlers.set(name, opts.handler);
          return Promise.resolve(null);
        },
        invoke: () => null,
      },
      daemon: { call: async () => [], repoRoot: async () => '/repo' },
      fs: {
        stat: async (p: string) => ({ is_dir: DIRS.has(p) }),
        exists: async () => true,
        readDir: async () => [{ name: 'notes.txt', path: '/repo/album/notes.txt', is_dir: false }],
      },
      statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
      recent: { touch: vi.fn(async () => {}) },
      contextMenu: Object.assign(noop, {
        addDefaultItems: (provider: Provider) => void providers.push(provider),
      }),
    },
  };
}

const flush = async () => {
  for (let i = 0; i < 4; i += 1) await new Promise((r) => setTimeout(r, 0));
};

/** Runs the menu's "Copy" and answers what landed in the shared clipboard. */
function copiedBy(provider: Provider): string[] | undefined {
  const items = provider(new MouseEvent('contextmenu'));
  items.find((item) => item.label === 'Copy')?.action?.();
  return getClipboard()?.paths;
}

describe('file panel — context menu target', () => {
  beforeEach(() => {
    setClipboard(null);
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({}),
        arrayBuffer: async () => new ArrayBuffer(0),
      })),
    );
  });

  async function mountPanel(path: string, tracked = true) {
    const s = stub(path, tracked);
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file/main.js');
    await mod.mount(root, s.api as never);
    await flush();
    return { ...s, root, viewer: root.getElementById('viewer')! };
  }

  test('following the selection, the menu acts on the selected file', async () => {
    const { providers } = await mountPanel('/repo/album/notes.txt');
    expect(copiedBy(providers[0])).toEqual(['/repo/album/notes.txt']);
  });

  test('an untracked file has the file actions too, in the active repository', async () => {
    const { providers } = await mountPanel('/repo/album/notes.txt', false);
    expect(copiedBy(providers[0])).toEqual(['/repo/album/notes.txt']);
  });

  test('after drilling into a listing, the menu acts on the file shown', async () => {
    const { providers, viewer } = await mountPanel('/repo/album');
    (viewer.querySelector('.tile') as HTMLElement).click();
    await flush();
    expect(copiedBy(providers[0])).toEqual(['/repo/album/notes.txt']);
  });

  test('after file:back, the menu acts on the folder shown', async () => {
    const { providers, handlers } = await mountPanel('/repo/album/notes.txt');
    await handlers.get('file:back')!();
    await flush();
    expect(copiedBy(providers[0])).toEqual(['/repo/album']);
  });
});
