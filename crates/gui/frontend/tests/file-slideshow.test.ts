// `viewer:play` (doc "Slideshow"): the viewer panel walks a list of metarecords —
// the checked selection or the query the list shows — playing each audio or
// video to its end, showing each image for a while, and skipping the rest.
// It moves the workspace's selection, so every other panel follows.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { withRuns } from './support/runs';

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

type Handler = (...args: (string | undefined)[]) => unknown;

const KINDS: Record<string, string> = {
  pdf: 'document',
  png: 'image',
  gif: 'gif',
  mp4: 'video',
  mp3: 'audio',
};

// Each metarecord's file, relative to the repository root `/repo`.
const FILES: Record<string, string> = {
  u_img: '/a.png',
  u_doc: '/b.pdf',
  u_vid: '/c.mp4',
  u_snd: '/d.mp3',
  u_gif: '/e.gif',
  u_gone: '', // no file any more
};

// Short durations, so the slideshow runs in real time within a test.
const DEFAULTS = { slideshowImageSeconds: 0.03, slideshowGifSeconds: 0.03, slideshowGapSeconds: 0.01 };

function stub(vars: Record<string, unknown>) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const listeners = new Map<string, (value: unknown) => void>();
  const state = new Map<string, unknown>([
    ['selected_paths', []],
    ['selected_metarecord', null],
    ['active_repo', 'r'],
    ...Object.entries(vars),
  ]);
  const statusBar = {
    message: vi.fn(async (_text: string, _ms?: number) => {}),
    error: vi.fn(async () => {}),
  };
  const shown: string[] = []; // the uuids the selection moved to, in turn
  const calls: { method: string; url: string; body: unknown }[] = [];
  const set = vi.fn(async (key: string, value: unknown) => {
    state.set(key, value);
    if (key === 'selected_metarecord') shown.push((value as { uuid: string }).uuid);
    listeners.get(key)?.(value);
  });
  return {
    handlers,
    statusBar,
    shown,
    calls,
    state,
    /** Selects `uuid` by hand, as moving the list's cursor does. */
    select: async (uuid: string) => {
      state.set('selected_metarecord', { uuid, repo: 'r' });
      state.set('selected_paths', [`/repo${FILES[uuid]}`]);
      listeners.get('selected_paths')?.([`/repo${FILES[uuid]}`]);
    },
    api: {
      guiServer: 'http://127.0.0.1:7524',
      sessionToken: 'test-token',
      pageSize: 10,
      settings: {},
      defaults: DEFAULTS,
      whenVisible: (fn: () => void) => fn(),
      provide: () => {},
      workspace: {
        get: async (key: string) => state.get(key) ?? null,
        set,
        onChange: (key: string, fn: (value: unknown) => void) => void listeners.set(key, fn),
      },
      commands: {
        register: (name: string, opts: { handler?: Handler }) => {
          if (opts.handler) handlers.set(name, opts.handler);
          return Promise.resolve(null);
        },
        invoke: () => null,
      },
      daemon: {
        call: async (method: string, url: string, body?: unknown) => {
          calls.push({ method, url, body });
          if (url === '/repos/r/query') return ['u_vid', 'u_img'];
          if (url.endsWith('/fields/playback_position')) {
            return method === 'GET' ? { values: [{ type: 'float', value: 42 }] } : {};
          }
          return {};
        },
        repoRoot: async () => '/repo',
        treePaths: async (_repo: string, _field: string, uuids: string[]) =>
          Object.fromEntries(uuids.map((u) => [u, FILES[u] ? [FILES[u]] : []])),
      },
      fs: {
        stat: async (p: string) => ({ is_dir: false, kind: KINDS[p.split('.').pop() ?? ''] ?? null }),
        exists: async () => true,
        readDir: async () => [],
      },
      statusBar,
      recent: { touch: vi.fn(async () => {}) },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
  };
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const flush = async () => {
  for (let i = 0; i < 6; i += 1) await sleep(0);
};
/** Waits for `predicate`, polling the event loop (a test-only wait). */
async function until(predicate: () => boolean, ms = 1000) {
  const end = Date.now() + ms;
  while (!predicate()) {
    if (Date.now() > end) throw new Error('timed out');
    await sleep(2);
  }
}

const urlOf = (input: RequestInfo | URL): string =>
  typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;

const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
  const url = urlOf(input);
  if (url.includes('/fsraw')) return { ok: false, status: 404 } as unknown as Response;
  const body = url.includes('/__media-support')
    ? { audio: true, video: true, missing: [] }
    : { missing: [], slow: null };
  return { ok: true, json: async () => body } as unknown as Response;
});

describe('viewer panel — the slideshow', () => {
  beforeEach(() => {
    fetchMock.mockClear();
    vi.stubGlobal('fetch', fetchMock);
    vi.stubGlobal('createImageBitmap', async () => {
      throw new Error('no canvas in jsdom');
    });
    HTMLMediaElement.prototype.play = vi.fn(async () => {});
    HTMLMediaElement.prototype.pause = vi.fn(function (this: HTMLMediaElement) {});
    HTMLMediaElement.prototype.load = vi.fn();
    document.body.replaceChildren();
  });

  async function mountPanel(vars: Record<string, unknown> = {}) {
    const s = stub(vars);
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/viewer/main.js');
    await mod.mount(root, withRuns(s.api) as never);
    await flush();
    const viewer = root.getElementById('viewer')!;
    return {
      ...s,
      viewer,
      run: async (name: string, ...args: (string | undefined)[]) => {
        await s.handlers.get(name)!(...args);
        await flush();
      },
      media: () => viewer.querySelector<HTMLMediaElement>('video, audio'),
    };
  }

  test('the checked selection: an image waits, a document is skipped, a video plays to its end', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_img', 'u_doc', 'u_vid'] });
    await p.run('viewer:play', 'selection');
    expect(p.shown).toEqual(['u_img']);
    expect(p.viewer.querySelector('img')).not.toBeNull();

    // The image's time is up: the document is passed over without ever being
    // selected (nor shown, nor counted as viewed), and the video plays.
    await until(() => p.media() !== null);
    expect(p.shown).toEqual(['u_img', 'u_vid']);
    const video = p.media()!;
    expect(video.play).toHaveBeenCalled();

    // Nothing moves while the video plays.
    await sleep(60);
    expect(p.shown).toEqual(['u_img', 'u_vid']);

    video.dispatchEvent(new Event('ended'));
    await until(() => p.statusBar.message.mock.calls.some(([m]) => /finished/i.test(String(m))));
  });

  test('a video starts from the beginning, not where it was left', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_vid'] });
    await p.run('viewer:play', 'selection');
    await until(() => p.media() !== null);
    const video = p.media()!;
    Object.defineProperty(video, 'duration', { value: 100, configurable: true });
    video.dispatchEvent(new Event('loadedmetadata'));
    expect(video.currentTime).toBe(0);
  });

  test('the query shown: the list’s query and sort, in the order the daemon gives', async () => {
    const query = { type: 'eq', field: 'kind', value: { type: 'string', value: 'clip' } };
    const sort = [{ field: 'rating', order: 'desc' }];
    const p = await mountPanel({
      'metarecord-list:effective-query': query,
      'metarecord-list:sort': sort,
    });
    await p.run('viewer:play', 'query');
    const post = p.calls.find((c) => c.url === '/repos/r/query');
    expect(post?.body).toEqual({ query, sort });
    expect(p.shown).toEqual(['u_vid']);
  });

  test('loop starts over at the end', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_img', 'u_doc', 'u_gif'] });
    await p.run('viewer:play', 'selection', 'loop');
    await until(() => p.shown.length >= 4);
    expect(p.shown.slice(0, 4)).toEqual(['u_img', 'u_gif', 'u_img', 'u_gif']);
    await p.run('viewer:play', 'stop');
  });

  test('a list with nothing to show stops instead of spinning', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_doc', 'u_gone'] });
    await p.run('viewer:play', 'selection', 'loop');
    await until(() =>
      p.statusBar.message.mock.calls.some(([m]) => /nothing to play/i.test(String(m))),
    );
    const count = p.shown.length;
    await sleep(60);
    expect(p.shown.length).toBe(count);
  });

  test('an empty selection says so and plays nothing', async () => {
    const p = await mountPanel({ selected_metarecords: [] });
    await p.run('viewer:play', 'selection');
    expect(p.shown).toEqual([]);
    expect(p.statusBar.message.mock.calls.some(([m]) => /no metarecords are checked/i.test(String(m)))).toBe(true);
  });

  test('stop leaves the file shown and moves on no further', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_img', 'u_vid'] });
    await p.run('viewer:play', 'selection');
    await p.run('viewer:play', 'stop');
    await sleep(60);
    expect(p.shown).toEqual(['u_img']);
  });

  test('choosing a file by hand continues from it', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_img', 'u_doc', 'u_gif', 'u_snd'] });
    await p.run('viewer:play', 'selection');
    await p.select('u_gif'); // the user moves the list's cursor
    await until(() => p.media() !== null);
    // The GIF was shown (its length unreadable here: the fixed wait), then the
    // slideshow went on after it rather than back to the document.
    expect(p.shown).toEqual(['u_img', 'u_snd']);
  });

  test('play-pause holds an image on screen, and resumes', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_img', 'u_vid'] });
    await p.run('viewer:play', 'selection');
    await p.run('viewer:play-pause');
    await sleep(60);
    expect(p.shown).toEqual(['u_img']);
    await p.run('viewer:play-pause');
    await until(() => p.shown.length === 2);
  });

  test('walking up out of the file holds the slideshow until the selection moves', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_img', 'u_doc', 'u_vid'] });
    await p.run('viewer:play', 'selection');
    await p.run('viewer:back'); // the folder holding the image
    await sleep(80);
    expect(p.shown).toEqual(['u_img']);
    await p.select('u_doc'); // moving the selection takes it up again
    await until(() => p.media() !== null);
    expect(p.shown).toEqual(['u_img', 'u_vid']);
  });

  test('a file chosen by hand that cannot be shown is passed over at once', async () => {
    const p = await mountPanel({ selected_metarecords: ['u_img', 'u_doc', 'u_gif', 'u_vid'] });
    await p.run('viewer:play', 'selection');
    await p.select('u_doc');
    await until(() => p.shown.length === 2);
    expect(p.shown).toEqual(['u_img', 'u_gif']);
  });

  test('an unknown target is refused', async () => {
    const p = await mountPanel();
    await expect(async () => p.run('viewer:play', 'everything')).rejects.toThrow(/everything/);
  });
});
