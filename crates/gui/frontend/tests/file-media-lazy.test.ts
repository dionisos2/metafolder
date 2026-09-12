// The `file` panel never builds a media pipeline on sight (spec-gui "file panel
// type" / "Untrusted media"): opening a video or an audio file shows its poster
// frame — rendered out of process by ffmpeg and cached — and the <video>/<audio>
// element is created only when playback is actually asked for.
//
// The bug that motivates it: mounting the element makes WebKit's web process
// build a GStreamer pipeline, and one that stalls (a software decoder on a
// GPU-less machine, a 4K AV1 stream) takes the whole UI down with it — no
// repaint, no keys, the window has to be killed. A tagging walk, which shows one
// file per question, hit it reliably.

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
      fs: { stat: async () => ({ is_dir: false }), readDir: async () => [] },
      statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
      recent: { touch: vi.fn(async () => {}) },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
  };
}

/** Let the panel's async render settle. */
const flush = async () => {
  for (let i = 0; i < 4; i += 1) await new Promise((r) => setTimeout(r, 0));
};

/** The URL of a fetch argument. `String(input)` would stringify a `Request`
 *  through Object.prototype.toString ("[object Object]"). */
const urlOf = (input: RequestInfo | URL): string =>
  typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;

const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
  const url = urlOf(input);
  const body = url.includes('/__media-support')
    ? { audio: true, video: true, missing: [] }
    : { missing: [], slow: null };
  return { ok: true, json: async () => body } as unknown as Response;
});

describe('file panel — media loads only on demand', () => {
  beforeEach(() => {
    fetchMock.mockClear();
    vi.stubGlobal('fetch', fetchMock);
    // jsdom implements none of the media transport.
    HTMLMediaElement.prototype.play = vi.fn(async () => {});
    HTMLMediaElement.prototype.pause = vi.fn(function (this: HTMLMediaElement) {});
    HTMLMediaElement.prototype.load = vi.fn();
  });

  async function mountPanel(path: string) {
    const s = stub(path);
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file/main.js');
    await mod.mount(root, s.api as never);
    await flush();
    return { ...s, root, viewer: root.getElementById('viewer')! };
  }

  test('a video shows its poster frame, and no media element', async () => {
    const { viewer } = await mountPanel('/music/clip.mp4');
    expect(viewer.querySelector('video')).toBeNull();
    const poster = viewer.querySelector('img');
    expect(poster?.getAttribute('src')).toContain('/thumbnail?path=');
    expect(poster?.getAttribute('src')).toContain(encodeURIComponent('/music/clip.mp4'));
  });

  test('showing it probes nothing: no decoder is consulted until playback', async () => {
    await mountPanel('/music/clip.mp4');
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test('the poster says how to start playing', async () => {
    const { viewer } = await mountPanel('/music/clip.mp4');
    expect(viewer.textContent).toMatch(/play/i);
  });

  test('an audio file falls back to its type glyph, still with no element', async () => {
    const { viewer } = await mountPanel('/music/song.mp3');
    expect(viewer.querySelector('audio')).toBeNull();
    expect(viewer.textContent).toContain('🎵');
  });

  test('play/pause builds the element, probes the decoders, and plays', async () => {
    const { handlers, viewer } = await mountPanel('/music/clip.mp4');
    await handlers.get('file:play-pause')!();
    await flush();
    const video = viewer.querySelector('video');
    expect(video?.getAttribute('src')).toContain('/fsraw?path=');
    expect(fetchMock.mock.calls.some(([url]) => urlOf(url).includes('/__media-probe'))).toBe(true);
    expect(HTMLMediaElement.prototype.play).toHaveBeenCalled();
  });

  test('clicking the poster starts it too', async () => {
    const { viewer } = await mountPanel('/music/clip.mp4');
    viewer.querySelector('button')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await flush();
    expect(viewer.querySelector('video')).not.toBeNull();
  });

  test('once loaded, play/pause toggles the element instead of rebuilding it', async () => {
    const { handlers, viewer } = await mountPanel('/music/clip.mp4');
    const play = handlers.get('file:play-pause')!;
    await play();
    await flush();
    const video = viewer.querySelector('video')!;
    Object.defineProperty(video, 'paused', { value: false, configurable: true });
    await play();
    await flush();
    expect(viewer.querySelectorAll('video')).toHaveLength(1);
    expect(HTMLMediaElement.prototype.pause).toHaveBeenCalled();
  });

  test('a decoder the machine lacks is reported when playback is asked for', async () => {
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = urlOf(input);
      const body = url.includes('/__media-support')
        ? { audio: true, video: true, missing: [] }
        : { missing: ['H.264 decoder'], slow: null };
      return { ok: true, json: async () => body } as unknown as Response;
    });
    const { handlers, viewer } = await mountPanel('/music/clip.mp4');
    await handlers.get('file:play-pause')!();
    await flush();
    expect(viewer.querySelector('video')).toBeNull();
    expect(viewer.textContent).toContain('H.264 decoder');
  });
});
