// The `file` panel's volume and mute are panel state, not the media element's
// (spec-gui "Media transport"): the element only exists while something plays,
// so a level that lived on it could only ever be set *after* the sound had
// already come out at full blast. They are settable in front of the poster,
// carry to the element when it is built, and stay put while browsing files —
// like the zoom level.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/file');

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

/** A stub API showing `path`, with no repository (so nothing else is fetched). */
function stub(path: string) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const listeners = new Map<string, (value: unknown) => void>();
  const vars = new Map<string, unknown>([
    ['selected_paths', [path]],
    ['selected_metarecord', null],
    ['active_repo', null],
  ]);
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  return {
    handlers,
    listeners,
    statusBar,
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
        onChange: (key: string, fn: (value: unknown) => void) => void listeners.set(key, fn),
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
      statusBar,
      recent: { touch: vi.fn(async () => {}) },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
  };
}

/** Let the panel's async render settle. */
const flush = async () => {
  for (let i = 0; i < 4; i += 1) await new Promise((r) => setTimeout(r, 0));
};

const urlOf = (input: RequestInfo | URL): string =>
  typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;

const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
  const url = urlOf(input);
  const body = url.includes('/__media-support')
    ? { audio: true, video: true, missing: [] }
    : { missing: [], slow: null };
  return { ok: true, json: async () => body } as unknown as Response;
});

describe('file panel — the volume is set before playback, not after', () => {
  beforeEach(() => {
    fetchMock.mockClear();
    vi.stubGlobal('fetch', fetchMock);
    // jsdom implements none of the media transport.
    HTMLMediaElement.prototype.play = vi.fn(async () => {});
    HTMLMediaElement.prototype.pause = vi.fn(function (this: HTMLMediaElement) {});
    HTMLMediaElement.prototype.load = vi.fn();
    document.body.replaceChildren();
  });

  async function mountPanel(path: string) {
    const s = stub(path);
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file/main.js');
    await mod.mount(root, s.api as never);
    await flush();
    const viewer = root.getElementById('viewer')!;
    return {
      ...s,
      root,
      viewer,
      run: async (name: string) => {
        await s.handlers.get(name)!();
        await flush();
      },
      /** Browse to another file, as metarecord-list's selection does. */
      show: async (next: string) => {
        s.listeners.get('selected_paths')?.([next]);
        await flush();
      },
      video: () => viewer.querySelector('video'),
    };
  }

  test('turning it down in front of the poster carries into the player', async () => {
    const p = await mountPanel('/music/clip.mp4');
    expect(p.video()).toBeNull(); // nothing mounted: the poster is showing

    await p.run('file:volume-down');
    await p.run('file:volume-down');

    expect(p.statusBar.message).toHaveBeenCalledWith(
      expect.stringContaining('80%'),
      expect.anything(),
    );

    await p.run('file:play-pause');
    expect(p.video()?.volume).toBeCloseTo(0.8);
  });

  test('the poster says what the volume will be', async () => {
    const p = await mountPanel('/music/clip.mp4');
    expect(p.viewer.textContent).not.toMatch(/volume/i); // 100%: nothing to say

    await p.run('file:volume-down');

    expect(p.viewer.textContent).toMatch(/90\s*%/);
  });

  test('muting before playback starts the player muted', async () => {
    const p = await mountPanel('/music/clip.mp4');

    await p.run('file:mute');
    expect(p.viewer.textContent).toMatch(/muted/i);

    await p.run('file:play-pause');
    expect(p.video()?.muted).toBe(true);
  });

  test('the level carries to the next file', async () => {
    const p = await mountPanel('/music/clip.mp4');
    await p.run('file:volume-down');
    await p.run('file:play-pause');
    expect(p.video()?.volume).toBeCloseTo(0.9);

    await p.show('/music/other.mp4');
    expect(p.video()).toBeNull(); // the next file is behind its own poster
    await p.run('file:play-pause');

    expect(p.video()?.volume).toBeCloseTo(0.9);
  });

  test('a mounted element still follows the volume commands', async () => {
    const p = await mountPanel('/music/clip.mp4');
    await p.run('file:play-pause');

    await p.run('file:volume-down');
    expect(p.video()?.volume).toBeCloseTo(0.9);

    await p.run('file:mute');
    expect(p.video()?.muted).toBe(true);
    await p.run('file:volume-up'); // louder undoes a mute
    expect(p.video()?.muted).toBe(false);
    expect(p.video()?.volume).toBeCloseTo(1);
  });
});
