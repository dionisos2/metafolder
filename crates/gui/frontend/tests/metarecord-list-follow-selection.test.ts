// The metarecord list follows a selection made elsewhere (doc "Cross-panel
// selection"): when another panel — the file panel's slideshow, the recent
// panel — selects a metarecord the list has loaded, its cursor moves there, so
// the arrows go on from the file on screen. It also publishes its sort beside
// its query, for whoever plays what it shows (doc "Slideshow").

import { describe, expect, test, vi, beforeEach } from 'vitest';
import { cursor } from './support/cursor';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-list');

/** The shell's mount path: the panel's body (minus scripts/styles) into a Shadow root. */
function shadowFor(): ShadowRoot {
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

type Handler = (arg?: unknown) => unknown;
type QueryCall = { cursor: string | null; limit: number };

const PAGE = 3;

/** Records of one page, named m1..mN over the whole (paged) result. */
function pageOf(index: number) {
  return Array.from({ length: PAGE }, (_, i) => ({
    uuid: `m${index * PAGE + i + 1}`,
    version: 1,
    fields: [],
  }));
}

/** A stub API over a two-page daemon result (6 records, page size 3). */
function stubApi(
  handlers: Map<string, Handler>,
  calls: QueryCall[],
  pages: number,
  { displayed = true, echoLate = false } = {},
) {
  const noop = () => {};
  const store = new Map<string, unknown>([['active_repo', 'r']]);
  const listeners = new Map<string, (value: unknown) => void>();
  const echoes: (() => void)[] = [];
  return {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-list',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: PAGE,
    settings: {},
    defaults: {},
    visible: false, // never displayed: the scroll pager can never fire
    onVisibility: noop,
    // …but the panel still ran its query — unless it is never displayed at all.
    whenVisible: (fn: () => void) => (displayed ? fn() : undefined),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      query: async (_repo: string, body: { cursor?: string; limit: number }) => {
              const cursor = body.cursor ?? null;
              calls.push({ cursor, limit: body.limit });
              const index = cursor === null ? 0 : Number(cursor);
              return {
                records: pageOf(index),
                nextCursor: index + 1 < pages ? String(index + 1) : null,
                total: pages * PAGE,
              };
            },
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
      metarecords: async () => new Map(),
      fields: async () => [],
      request: async () => ({ status: 200, body: null }),
      call: async () => null,
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async () => [],
    },
    changes: { sync: async () => {}, subscribe: () => () => {} },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: {},
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => {
        store.set(key, value);
        // The GUI echoes every write to every listener — the writer's too — as
        // an event that may come after the write itself has answered.
        if (echoLate) echoes.push(() => listeners.get(key)?.(value));
      },
      adoptRepo: async () => {},
      onChange: (key: string, fn: (value: unknown) => void) => void listeners.set(key, fn),
    },
    provide: vi.fn(),
    commands: {
      register: async (name: string, spec: { handler: Handler }) => {
        handlers.set(name, spec.handler);
        return null;
      },
      invoke: () => null,
    },
    addKeybinding: async () => null,
    fs: { readDir: async () => [], stat: async () => ({}), exists: async () => true, homeDir: async () => '/home/user' },
    trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    store,
    listeners,
    echoes,
  };
}

async function mountPanel(pages: number, options: { displayed?: boolean; echoLate?: boolean } = {}) {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const calls: QueryCall[] = [];
  const api = stubApi(handlers, calls, pages, options);
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  return {
    shadow,
    calls,
    /** The uuid the panel last propagated as the selection. */
    selected: () =>
      (api.store.get('selected_metarecord') as { uuid: string } | undefined)?.uuid ?? null,
    rows: () => shadow.querySelectorAll('#rows tr').length,
    store: api.store,
    /** Delivers the oldest echo held back. */
    releaseEcho: async () => {
      api.echoes.shift()?.();
      await new Promise((r) => setTimeout(r, 0));
    },
    /** The selection set by another panel (the file panel's slideshow, a click
     *  in the recent panel): the variable changes under the list. */
    selectElsewhere: async (uuid: string) => {
      api.store.set('selected_metarecord', { uuid, repo: 'r' });
      api.listeners.get('selected_metarecord')?.({ uuid, repo: 'r' });
      await new Promise((r) => setTimeout(r, 0));
    },
    cursorRow: () => [...shadow.querySelectorAll('#rows tr')].findIndex((tr) => tr.classList.contains('cursor')),
    provide: api.provide,
    /** `cursor:next` on the list (doc "Generic panel commands"). */
    next: async () => {
      await cursor(api.provide, 'next');
      await new Promise((r) => setTimeout(r, 0)); // selection propagation
    },
  };
}

describe('a selection made elsewhere', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
    if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};
  });

  test('moves the cursor onto a loaded row, and the arrows go on from it', async () => {
    const p = await mountPanel(2);
    expect(p.cursorRow()).toBe(0);

    await p.selectElsewhere('m3');
    expect(p.cursorRow()).toBe(2);

    await p.next();
    expect(p.selected()).toBe('m4');
  });

  test('the late echo of the list’s own selection does not pull the cursor back', async () => {
    const p = await mountPanel(2, { echoLate: true });
    await p.next(); // m2, its echo held back
    await p.next(); // m3
    expect(p.cursorRow()).toBe(2);
    for (let i = 0; i < 8; i++) {
      await p.releaseEcho(); // m2's echo first, then the rest
      expect(p.cursorRow()).toBe(2);
    }
  });

  test('leaves the cursor alone for a metarecord the list has not loaded', async () => {
    const p = await mountPanel(2);
    await p.selectElsewhere('m2');
    await p.selectElsewhere('elsewhere');
    expect(p.cursorRow()).toBe(1);
  });
});

describe('the sort, published with the query', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
    if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};
  });

  test('is the empty list until a column is sorted', async () => {
    const p = await mountPanel(1);
    expect(p.store.get('metarecord-list:sort')).toEqual([]);
  });
});
