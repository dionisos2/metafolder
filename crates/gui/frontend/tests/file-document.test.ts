// The `file` panel previews a document (PDF) as server-rendered page images
// (spec-gui "Documents"). Two things are load-bearing here:
//
//  - the document's own bytes never reach the WebView. `/fsraw` carries the
//    session token in its URL, so a PDF loaded *as a document* (an <iframe>,
//    where WebKit's built-in PDF.js would take over) would run as code in the
//    GUI server's origin and could lift that token — the invariant guarded by
//    `tests/panel_invariants.rs`. The panel shows an <img> at `/document`, a
//    PNG poppler rendered out of process under bwrap.
//  - page navigation is a plain image swap, so the zoom state and the scroll
//    position machinery of the image viewer apply unchanged.

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

type Handler = (...args: string[]) => unknown;

function stub(path: string) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const vars = new Map<string, unknown>([
    ['selected_paths', [path]],
    ['selected_metarecord', null],
    ['active_repo', null],
  ]);
  return {
    handlers,
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
      daemon: { call: async () => ({}), repoRoot: async () => '/' },
      fs: { stat: async () => ({ is_dir: false }), exists: async () => true, readDir: async () => [] },
      statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
      recent: { touch: vi.fn(async () => {}) },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
  };
}

const flush = async () => {
  for (let i = 0; i < 4; i += 1) await new Promise((r) => setTimeout(r, 0));
};

const urlOf = (input: RequestInfo | URL): string =>
  typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;

/** Answers `/document/info` with `pages`; anything else is a plain ok. */
function mockFetch(pages: number | null) {
  return vi.fn(async (input: RequestInfo | URL) => {
    const url = urlOf(input);
    if (url.includes('/document/info')) {
      if (pages === null) return { ok: false, status: 500, json: async () => ({}) } as Response;
      return { ok: true, status: 200, json: async () => ({ pages }) } as unknown as Response;
    }
    return { ok: true, status: 200, json: async () => ({}) } as unknown as Response;
  });
}

describe('file panel — document preview', () => {
  let fetchMock: ReturnType<typeof mockFetch>;

  beforeEach(() => {
    fetchMock = mockFetch(3);
    vi.stubGlobal('fetch', fetchMock);
  });

  async function mountPanel(path: string) {
    const s = stub(path);
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file/main.js');
    await mod.mount(root, s.api as never);
    await flush();
    return { ...s, root, viewer: root.getElementById('viewer')! };
  }

  test('a PDF is shown as a rendered page image, never as its own bytes', async () => {
    const { viewer } = await mountPanel('/docs/report.pdf');
    const img = viewer.querySelector('img');
    const src = img?.getAttribute('src') ?? '';
    expect(src).toContain('/document?path=');
    expect(src).toContain(encodeURIComponent('/docs/report.pdf'));
    expect(src).toContain('page=1');
    // The invariant: no /fsraw anywhere in the document view.
    expect(viewer.innerHTML).not.toContain('/fsraw');
  });

  test('the page count is read once and shown in the toolbar', async () => {
    const { root } = await mountPanel('/docs/report.pdf');
    const infoCalls = fetchMock.mock.calls.filter(([url]) =>
      urlOf(url).includes('/document/info'),
    );
    expect(infoCalls).toHaveLength(1);
    const pager = root.getElementById('doc-pager')!;
    expect(pager.hidden).toBe(false);
    expect(root.getElementById('page-label')!.textContent).toBe('1 / 3');
  });

  test('next/prev walk the pages and stop at both ends', async () => {
    const { handlers, root, viewer } = await mountPanel('/docs/report.pdf');
    const page = handlers.get('file:page')!;
    const next = () => page('next');
    const prev = () => page('prev');
    const src = () => viewer.querySelector('img')!.getAttribute('src') ?? '';

    await next();
    await flush();
    expect(src()).toContain('page=2');
    expect(root.getElementById('page-label')!.textContent).toBe('2 / 3');

    await next();
    await next(); // past the last page: stays on it
    await flush();
    expect(src()).toContain('page=3');

    await prev();
    await flush();
    expect(src()).toContain('page=2');
    await prev();
    await prev(); // before the first page: stays on it
    await flush();
    expect(src()).toContain('page=1');
  });

  test('first/last jump to the ends', async () => {
    const { handlers, viewer } = await mountPanel('/docs/report.pdf');
    await handlers.get('file:page')!('last');
    await flush();
    expect(viewer.querySelector('img')!.getAttribute('src')).toContain('page=3');
    await handlers.get('file:page')!('first');
    await flush();
    expect(viewer.querySelector('img')!.getAttribute('src')).toContain('page=1');
  });

  test('turning a page swaps the image rather than rebuilding the view', async () => {
    const { handlers, viewer } = await mountPanel('/docs/report.pdf');
    const before = viewer.querySelector('img');
    await handlers.get('file:page')!('next');
    await flush();
    // Same element: the zoom target, and any zoom the user set, survive.
    expect(viewer.querySelector('img')).toBe(before);
  });

  test('the page commands are no-ops in front of anything else', async () => {
    const { handlers, root, viewer } = await mountPanel('/photos/pic.png');
    expect(root.getElementById('doc-pager')!.hidden).toBe(true);
    const src = viewer.querySelector('img')!.getAttribute('src');
    await handlers.get('file:page')!('next');
    await flush();
    expect(viewer.querySelector('img')!.getAttribute('src')).toBe(src);
  });

  test('a document that cannot be rendered says so instead of showing nothing', async () => {
    fetchMock = mockFetch(null);
    vi.stubGlobal('fetch', fetchMock);
    const { root, viewer } = await mountPanel('/docs/report.pdf');
    expect(viewer.querySelector('img')).toBeNull();
    expect(viewer.textContent).toMatch(/no preview/i);
    expect(root.getElementById('doc-pager')!.hidden).toBe(true);
  });
});
