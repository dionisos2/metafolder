// Going back in the `file` panel (spec-gui "file panel type"): Backspace steps
// up one level — out of a drilled-in listing, or from the followed selection
// into the folder holding the previewed file — the way it does in the file
// manager. `backTarget` is the pure decision; the mount test checks it is
// actually wired to a command.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { backTarget } from '../../default-config/panel-types/file/main.js';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/file');

describe('backTarget', () => {
  test('from the followed selection, goes up to the containing folder', () => {
    expect(backTarget(null, '/music/album/song.mp3')).toEqual({
      move: true,
      path: '/music/album',
    });
  });

  test('out of a drill-in, goes up one level', () => {
    expect(backTarget('/music/album/live/take.mp3', '/music')).toEqual({
      move: true,
      path: '/music/album/live',
    });
  });

  test('reaching the selected path again resumes following the selection', () => {
    expect(backTarget('/music/album', '/music')).toEqual({ move: true, path: null });
  });

  test('the filesystem root has nowhere to go', () => {
    expect(backTarget('/', '/music')).toEqual({ move: false });
    expect(backTarget(null, '/')).toEqual({ move: false });
  });

  test('nothing is shown: nothing to go back from', () => {
    expect(backTarget(null, undefined)).toEqual({ move: false });
  });
});

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

/** A stub API: the panel never becomes visible, so nothing is streamed — the
 *  navigation state (and the path bar it paints) is all this test needs. */
function stub(selectedPaths: string[]) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const vars = new Map<string, unknown>([
    ['selected_paths', selectedPaths],
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
      // The panel is never displayed: renderViewer is deferred, so a `back`
      // only moves the state and repaints the path bar.
      whenVisible: noop,
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
      fs: { stat: async () => ({ is_dir: false }), readDir: async () => [] },
      statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
      recent: { touch: vi.fn(async () => {}) },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
  };
}

describe('file panel — file:back', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
  });

  async function mountPanel(selectedPaths: string[]) {
    const s = stub(selectedPaths);
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file/main.js');
    await mod.mount(root, s.api as never);
    await new Promise((r) => setTimeout(r, 0));
    return { ...s, root };
  }

  test('steps up out of the previewed file, then out of its folder', async () => {
    const { handlers, root } = await mountPanel(['/music/album/song.mp3']);
    const back = handlers.get('file:back');
    expect(back).toBeDefined();

    await back!();
    expect(root.getElementById('path-bar')!.textContent).toContain('/music/album');
    await back!();
    expect(root.getElementById('path-bar')!.textContent).toContain('/music');
  });

  test('with nothing selected it does nothing', async () => {
    const { handlers, root } = await mountPanel([]);
    await handlers.get('file:back')!();
    expect(root.getElementById('path-bar')!.textContent).toBe('');
  });
});
