// metarecord-list keyboard paging: `metarecord-list:next` at the end of the
// loaded rows pulls the next page instead of stopping there.
//
// The scroll-driven pager only fires on a real scroll event, so a panel that is
// not displayed (driven by a script or a keybinding from the other slot) could
// never get past the first page. Moving the selection past the last loaded row
// must load more on its own.

import { describe, expect, test, vi, beforeEach } from 'vitest';
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
function stubApi(handlers: Map<string, Handler>, calls: QueryCall[], pages: number) {
  const noop = () => {};
  const store = new Map<string, unknown>([['active_repo', 'r']]);
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
    whenVisible: (fn: () => void) => fn(), // …but the panel still ran its query
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      request: async () => ({ status: 200, body: null }),
      call: async () => null,
      parseQuery: async () => null,
      expandQuery: async () => '',
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      invalidatePath: () => true,
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async () => [],
    },
    cache: {
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
      fetchMetarecords: async () => {},
      fetchTreeRefs: async () => {},
      fetchFields: async () => {},
      readMetarecord: () => null,
      readTreeRef: () => [],
      readFields: () => [],
      fieldType: () => null,
      sync: async () => {},
      subscribe: () => () => {},
      REFRESH: Symbol('refresh'),
    },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: { pickerSeed: async () => null },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: noop,
    },
    commands: {
      register: async (name: string, spec: { handler: Handler }) => {
        handlers.set(name, spec.handler);
        return null;
      },
      invoke: () => null,
    },
    addKeybinding: async () => null,
    fs: { readDir: async () => [], stat: async () => ({}), homeDir: async () => '/home/user' },
    trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    store,
  };
}

async function mountPanel(pages: number) {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const calls: QueryCall[] = [];
  const api = stubApi(handlers, calls, pages);
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
    invoke: async (name: string, arg?: unknown) => {
      const h = handlers.get(name);
      if (!h) throw new Error(`command not registered: ${name}`);
      await h(arg);
      await new Promise((r) => setTimeout(r, 0)); // selection propagation
    },
  };
}

describe('metarecord-list:next at the end of the loaded rows', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
    // jsdom has no scrollIntoView, which the panel calls when moving the cursor.
    if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};
  });

  test('loads the next page and lands on its first row', async () => {
    const p = await mountPanel(2);
    expect(p.calls).toHaveLength(1);
    expect(p.rows()).toBe(PAGE);
    expect(p.selected()).toBe('m1');

    await p.invoke('metarecord-list:next'); // m2
    await p.invoke('metarecord-list:next'); // m3 — the last loaded row
    expect(p.calls).toHaveLength(1);
    expect(p.selected()).toBe('m3');

    await p.invoke('metarecord-list:next'); // past it: pull page 2

    expect(p.calls).toHaveLength(2);
    expect(p.calls[1].cursor).toBe('1'); // page 2
    expect(p.rows()).toBe(2 * PAGE);
    expect(p.selected()).toBe('m4');
  });

  test('stops on the last row once the daemon has no more', async () => {
    const p = await mountPanel(1);

    for (let i = 0; i < 5; i++) await p.invoke('metarecord-list:next');

    expect(p.calls).toHaveLength(1); // no cursor: nothing more to ask for
    expect(p.selected()).toBe('m3');
  });
});
